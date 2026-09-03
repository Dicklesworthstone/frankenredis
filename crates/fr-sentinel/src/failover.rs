#![forbid(unsafe_code)]

use crate::{
    FailoverState, INFO_PERIOD_MS, InstanceFlags, LinkStatus, PING_PERIOD_MS,
    SentinelRedisInstance, SentinelState,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlaveScore {
    pub key: String,
    pub priority: u32,
    pub repl_offset: u64,
    pub runid: Option<String>,
    pub is_connected: bool,
    pub master_link_up: bool,
}

impl SlaveScore {
    pub fn from_instance(key: &str, slave: &SentinelRedisInstance) -> Self {
        let master_link_up = slave.slave_master_link_status == LinkStatus::Up; // ubs:ignore - enum status comparison, not a secret
        Self {
            key: key.to_string(),
            priority: slave.slave_priority,
            repl_offset: slave.slave_repl_offset,
            runid: slave.runid.clone(),
            is_connected: !slave.link.disconnected,
            master_link_up,
        }
    }
}

pub fn select_slave(master: &SentinelRedisInstance) -> Option<String> {
    select_slave_at(master, 0, PING_PERIOD_MS, INFO_PERIOD_MS)
}

#[cfg_attr(feature = "bench-reference", inline(never))]
pub fn select_slave_at(
    master: &SentinelRedisInstance,
    now_ms: u64,
    ping_period_ms: u64,
    info_period_ms: u64,
) -> Option<String> {
    let mut best = None;
    for (key, slave) in &master.slaves {
        if !is_slave_eligible(master, slave, now_ms, ping_period_ms, info_period_ms) {
            continue;
        }
        match best {
            Some((_, current))
                if compare_slave_instances(slave, current) != std::cmp::Ordering::Less => {} // ubs:ignore - rank ordering comparison, not a secret
            _ => best = Some((key, slave)),
        }
    }
    best.map(|(key, _)| key.clone())
}

#[cfg(feature = "bench-reference")]
#[inline(never)]
pub fn bench_select_slave_sort_all_reference(
    master: &SentinelRedisInstance,
    now_ms: u64,
    ping_period_ms: u64,
    info_period_ms: u64,
) -> Option<String> {
    let mut candidates: Vec<SlaveScore> = master
        .slaves
        .iter()
        .filter(|(_, slave)| {
            is_slave_eligible(master, slave, now_ms, ping_period_ms, info_period_ms)
        })
        .map(|(key, slave)| SlaveScore::from_instance(key, slave))
        .collect();

    if candidates.is_empty() {
        return None;
    }

    candidates.sort_by(compare_slaves);

    Some(candidates[0].key.clone())
}

fn is_slave_eligible(
    master: &SentinelRedisInstance,
    slave: &SentinelRedisInstance,
    now_ms: u64,
    ping_period_ms: u64,
    info_period_ms: u64,
) -> bool {
    if slave.is_s_down() || slave.is_o_down() {
        return false;
    }
    if slave.link.disconnected {
        return false;
    }
    if slave.slave_priority == 0 {
        return false;
    }
    if !slave.flags.contains(InstanceFlags::SLAVE) {
        return false;
    }

    if now_ms.saturating_sub(slave.link.last_avail_time) > ping_period_ms.saturating_mul(5) {
        return false;
    }

    let info_validity_time = if master.flags.contains(InstanceFlags::S_DOWN) {
        ping_period_ms.saturating_mul(5)
    } else {
        info_period_ms.saturating_mul(3)
    };
    if now_ms.saturating_sub(slave.info_refresh) > info_validity_time {
        return false;
    }

    let mut max_master_down_time = master.down_after_period.saturating_mul(10);
    if master.flags.contains(InstanceFlags::S_DOWN) {
        max_master_down_time =
            max_master_down_time.saturating_add(now_ms.saturating_sub(master.s_down_since_time));
    }
    if slave.master_link_down_time > max_master_down_time {
        return false;
    }

    true
}

#[cfg(feature = "bench-reference")]
fn compare_slaves(a: &SlaveScore, b: &SlaveScore) -> std::cmp::Ordering {
    if a.priority != b.priority {
        return a.priority.cmp(&b.priority);
    }

    if a.repl_offset != b.repl_offset {
        return b.repl_offset.cmp(&a.repl_offset);
    }

    match (&a.runid, &b.runid) {
        (Some(ra), Some(rb)) => cmp_ascii_case_insensitive(ra, rb),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    }
}

fn compare_slave_instances(
    a: &SentinelRedisInstance,
    b: &SentinelRedisInstance,
) -> std::cmp::Ordering {
    if a.slave_priority != b.slave_priority {
        return a.slave_priority.cmp(&b.slave_priority);
    }

    if a.slave_repl_offset != b.slave_repl_offset {
        return b.slave_repl_offset.cmp(&a.slave_repl_offset);
    }

    match (&a.runid, &b.runid) {
        (Some(ra), Some(rb)) => cmp_ascii_case_insensitive(ra, rb),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    }
}

fn cmp_ascii_case_insensitive(left: &str, right: &str) -> std::cmp::Ordering {
    for (left_byte, right_byte) in left.bytes().zip(right.bytes()) {
        let ordering = left_byte
            .to_ascii_lowercase()
            .cmp(&right_byte.to_ascii_lowercase());
        if ordering != std::cmp::Ordering::Equal {
            return ordering;
        }
    }
    left.len().cmp(&right.len())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FailoverEvent {
    StartFailover,
    SlaveSelected(String),
    SlaveofNoOneSent,
    PromotionConfirmed,
    ReconfigurationComplete,
    Timeout,
    Abort(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailoverContext {
    pub promoted_slave_key: Option<String>,
    pub slaves_to_reconfig: Vec<String>,
    pub slaves_reconfigured: Vec<String>,
}

impl Default for FailoverContext {
    fn default() -> Self {
        Self::new()
    }
}

impl FailoverContext {
    pub fn new() -> Self {
        Self {
            promoted_slave_key: None,
            slaves_to_reconfig: Vec::new(),
            slaves_reconfigured: Vec::new(),
        }
    }
}

pub fn advance_failover_state(
    master: &mut SentinelRedisInstance,
    event: FailoverEvent,
    ctx: &mut FailoverContext,
    now: u64,
) -> FailoverState {
    let current = master.failover_state;
    let next = match (current, event) {
        (FailoverState::None, FailoverEvent::StartFailover) => {
            master.failover_start_time = now;
            master.failover_state_change_time = now;
            FailoverState::WaitStart
        }

        (
            FailoverState::WaitStart | FailoverState::SelectSlave,
            FailoverEvent::SlaveSelected(key),
        ) => {
            if promote_selected_slave(master, &key, ctx) {
                master.failover_state_change_time = now;
                FailoverState::SendSlaveofNoone
            } else {
                current
            }
        }

        (FailoverState::SendSlaveofNoone, FailoverEvent::SlaveofNoOneSent) => {
            master.failover_state_change_time = now;
            FailoverState::WaitPromotion
        }

        (FailoverState::WaitPromotion, FailoverEvent::PromotionConfirmed) => {
            master.failover_state_change_time = now;
            FailoverState::ReconfSlaves
        }

        (FailoverState::ReconfSlaves, FailoverEvent::ReconfigurationComplete) => {
            master.failover_state_change_time = now;
            FailoverState::UpdateConfig
        }

        (_, FailoverEvent::Timeout | FailoverEvent::Abort(_)) => {
            clear_aborted_failover(master, now);
            FailoverState::None
        }

        _ => current,
    };

    master.failover_state = next;
    next
}

fn promote_selected_slave(
    master: &mut SentinelRedisInstance,
    key: &str,
    ctx: &mut FailoverContext,
) -> bool {
    let Some(slave) = master.slaves.get_mut(key) else {
        return false;
    };
    slave.flags.insert(InstanceFlags::PROMOTED);
    master.promoted_slave = Some(Box::new(slave.clone()));
    ctx.promoted_slave_key = Some(key.to_string());
    ctx.slaves_to_reconfig = master
        .slaves
        .iter()
        .filter(|(candidate, slave)| {
            candidate.as_str() != key && !slave.flags.contains(InstanceFlags::S_DOWN)
        })
        .map(|(candidate, _)| candidate.clone())
        .collect();
    true
}

fn clear_aborted_failover(master: &mut SentinelRedisInstance, now: u64) {
    master.failover_state_change_time = now;
    master.flags.remove(InstanceFlags::FAILOVER_IN_PROGRESS);
    master.flags.remove(InstanceFlags::FORCE_FAILOVER);
    if let Some(promoted) = master.promoted_slave.as_mut() {
        promoted.flags.remove(InstanceFlags::PROMOTED);
    }
    for slave in master.slaves.values_mut() {
        slave.flags.remove(InstanceFlags::PROMOTED);
    }
    master.promoted_slave = None;
}

pub fn check_failover_timeout(master: &SentinelRedisInstance, now: u64) -> bool {
    if master.failover_state == FailoverState::None {
        return false;
    }
    let timeout_base = if master.failover_state == FailoverState::WaitStart
        || master.failover_state_change_time == 0
    {
        master.failover_start_time
    } else {
        master.failover_state_change_time
    };
    now.saturating_sub(timeout_base) > master.failover_timeout
}

pub fn should_start_failover(master: &SentinelRedisInstance, _is_leader: bool, now: u64) -> bool {
    if !master.is_o_down() {
        return false;
    }
    if master.flags.contains(InstanceFlags::FAILOVER_IN_PROGRESS) {
        return false;
    }
    if master.failover_state != FailoverState::None {
        return false;
    }
    if master.failover_start_time != 0
        && now.saturating_sub(master.failover_start_time)
            < master.failover_timeout.saturating_mul(2)
    {
        return false;
    }
    true
}

pub fn generate_slaveof_command(master_ip: &str, master_port: u16) -> Vec<Vec<u8>> {
    vec![
        b"SLAVEOF".to_vec(),
        master_ip.as_bytes().to_vec(),
        master_port.to_string().into_bytes(),
    ]
}

pub fn generate_slaveof_no_one() -> Vec<Vec<u8>> {
    vec![b"SLAVEOF".to_vec(), b"NO".to_vec(), b"ONE".to_vec()]
}

pub fn track_slave_reconfiguration(
    ctx: &mut FailoverContext,
    slave_key: &str,
    status: ReconfigStatus,
) {
    match status {
        ReconfigStatus::Sent => {}
        ReconfigStatus::InProgress => {}
        ReconfigStatus::Done => {
            if !ctx.slaves_reconfigured.contains(&slave_key.to_string()) {
                ctx.slaves_reconfigured.push(slave_key.to_string());
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReconfigStatus {
    Sent,
    InProgress,
    Done,
}

pub fn is_reconfiguration_complete(ctx: &FailoverContext) -> bool {
    ctx.slaves_to_reconfig
        .iter()
        .all(|k| ctx.slaves_reconfigured.contains(k))
}

pub fn finalize_failover(
    state: &mut SentinelState,
    master_name: &str,
    ctx: &FailoverContext,
    now: u64,
) {
    let current_epoch = state.current_epoch;
    if let Some(master) = state.get_master_mut(master_name)
        && let Some(ref promoted_key) = ctx.promoted_slave_key
        && let Some(promoted) = master.slaves.remove(promoted_key)
    {
        let old_addr = master.addr.clone();
        let new_addr = promoted.addr.clone();
        let mut slave_addrs: Vec<_> = master
            .slaves
            .values()
            .filter(|slave| !sentinel_addr_eq(&slave.addr, &new_addr))
            .map(|slave| slave.addr.clone())
            .collect();
        if !sentinel_addr_eq(&old_addr, &new_addr) {
            push_unique_addr(&mut slave_addrs, old_addr);
        }

        let quorum = master.quorum;
        let down_after_period = master.down_after_period;
        master.slaves.clear();
        master.addr = new_addr;
        master.runid = None;
        master.config_epoch = if master.failover_epoch == 0 {
            current_epoch
        } else {
            master.failover_epoch
        };
        master.flags = InstanceFlags::MASTER;
        master.leader = None;
        master.failover_state = FailoverState::None;
        master.failover_state_change_time = 0;
        master.failover_start_time = 0;
        master.promoted_slave = None;
        master.s_down_since_time = 0;
        master.o_down_since_time = 0;
        master.link.act_ping_time = now;
        master.link.last_ping_time = 0;
        master.link.last_avail_time = now;
        master.link.last_pong_time = now;
        master.role_reported_time = now;
        master.role_reported = crate::Role::Master;

        for addr in slave_addrs {
            let key = sentinel_slave_key(&addr);
            let slave = reset_slave_instance(&key, addr, quorum, down_after_period, now);
            master.slaves.insert(key, slave);
        }
    }
}

fn sentinel_addr_eq(left: &crate::SentinelAddr, right: &crate::SentinelAddr) -> bool {
    left.port == right.port
        && (left.ip.eq_ignore_ascii_case(&right.ip)
            || left.hostname.eq_ignore_ascii_case(&right.hostname))
}

fn push_unique_addr(addrs: &mut Vec<crate::SentinelAddr>, addr: crate::SentinelAddr) {
    if !addrs
        .iter()
        .any(|existing| sentinel_addr_eq(existing, &addr))
    {
        addrs.push(addr);
    }
}

pub(crate) fn sentinel_slave_key(addr: &crate::SentinelAddr) -> String {
    format!("{}:{}", addr.hostname, addr.port)
}

pub(crate) fn reset_slave_instance(
    key: &str,
    addr: crate::SentinelAddr,
    quorum: u32,
    down_after_period: u64,
    now: u64,
) -> SentinelRedisInstance {
    let mut slave = SentinelRedisInstance::new_master(key, addr, quorum);
    slave.flags = InstanceFlags::SLAVE;
    slave.down_after_period = down_after_period;
    slave.initialize_created_link_state(now);
    slave
}

/// One socket operation fr-server performs on the sentinel's behalf: the
/// state machine above is pure, so every `SLAVEOF` it wants written to a real
/// instance comes out of [`failover_step`] as one of these, and the outcome is
/// folded back in through [`apply_failover_io_result`].
///
/// (frankenredis-rc-sentinel-failover) Before this existed the whole failover
/// module had no caller outside its own tests: a sentinel reached
/// `s_down,o_down` and then did nothing, and `SENTINEL FAILOVER` answered
/// NOGOODSLAVE against a healthy replica because replicas were discovered from
/// the master's INFO but never contacted, so `link.disconnected` never cleared.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FailoverIo {
    /// `SLAVEOF NO ONE` to the replica selected for promotion.
    PromoteReplica {
        master_name: String,
        replica_key: String,
        ip: String,
        port: u16,
    },
    /// `SLAVEOF <new_ip> <new_port>` to a replica that must follow the new
    /// master -- the remaining replicas during `ReconfSlaves`, and a former
    /// master that comes back reporting `role:master` after the failover.
    ReconfigureReplica {
        master_name: String,
        replica_key: String,
        ip: String,
        port: u16,
        new_ip: String,
        new_port: u16,
    },
}

/// Advance one master's failover by at most one state per call and return the
/// socket writes the server must perform. Mirrors `sentinelFailoverStateMachine`
/// plus `sentinelStartFailoverIfNeeded` in upstream sentinel.c, for the
/// single-sentinel case: the leader election is evaluated over the sentinels
/// this instance knows about with only its own vote, so a quorum that needs
/// peer votes does not start a failover here (that exchange is a separate
/// piece of work); `SENTINEL FAILOVER` (`FORCE_FAILOVER`) skips the election
/// exactly as upstream does.
///
/// Called once per probe tick, after the master and its replicas have been
/// probed, so `select_slave_at` sees fresh link and INFO state.
pub fn failover_step(state: &mut SentinelState, master_name: &str, now: u64) -> Vec<FailoverIo> {
    let my_runid = state.myid_hex();
    let epoch = state.current_epoch;
    let ping_period = state.debug_config.ping_period;
    let info_period = state.debug_config.info_period;
    let publish_period = state.debug_config.publish_period;
    let mut ios = Vec::new();
    let mut bump_epoch = None;
    let mut evaluate_election = false;
    let mut finalize_ctx = None;
    {
        let Some(master) = state.masters.get_mut(master_name) else {
            return ios;
        };

        // A failover that outlived failover-timeout is abandoned; the backoff
        // in `should_start_failover` keeps the next attempt from starting at
        // once, as upstream's `failover_start_time` check does.
        if check_failover_timeout(master, now) {
            let mut ctx = FailoverContext::new();
            advance_failover_state(master, FailoverEvent::Timeout, &mut ctx, now);
            return ios;
        }

        if master.failover_state == FailoverState::None && should_start_failover(master, true, now)
        {
            // sentinelStartFailover: claim a new epoch and enter WaitStart. No
            // ballot is cast here; sentinelGetLeader casts it when the election
            // is evaluated below, so a candidate whose peers already back
            // someone else joins them instead of splitting the vote.
            let new_epoch = epoch.saturating_add(1);
            master.failover_epoch = new_epoch;
            master.flags.insert(InstanceFlags::FAILOVER_IN_PROGRESS);
            let mut ctx = FailoverContext::new();
            advance_failover_state(master, FailoverEvent::StartFailover, &mut ctx, now);
            // failover_start_time is the base of the election timeout and of
            // the 2 x failover-timeout backoff before the next attempt;
            // sentinelStartFailover offsets it by a per-sentinel delay so the
            // losers of one election do not all retry in the same second.
            master.failover_start_time = crate::commands::sentinel_failover_start_time(
                now,
                new_epoch,
                master_name,
                &my_runid,
            );
            bump_epoch = Some(new_epoch);
        }

        match master.failover_state {
            FailoverState::None => {
                // No failover in flight: an instance we track as a replica that
                // reports `role:master` is a former master that came back (or
                // an operator's manual promotion) and is told to follow the
                // master we believe in, the way upstream's INFO refresh does --
                // but only once the picture is stable (sentinelMasterLooksSane
                // plus the 4 x publish-period grace): our master must itself
                // answer INFO as a live master, and the instance must have
                // held the master role, undisturbed by any down flag, for that
                // long. Without the grace a follower sentinel that probed the
                // replica the leader had just promoted, before the leader's
                // hello announced the new master, demoted it straight back
                // under the dead master and the quorum never converged.
                let wait_time = publish_period.saturating_mul(4);
                let master_looks_sane = master.role_reported == crate::Role::Master
                    && !master.is_s_down()
                    && !master.is_o_down()
                    && now.saturating_sub(master.info_refresh) < info_period.saturating_mul(2);
                let target_ip = master.addr.ip.clone();
                let target_port = master.addr.port;
                for (key, slave) in &master.slaves {
                    let most_recent_down = slave.s_down_since_time.max(slave.o_down_since_time);
                    let no_down_for_grace =
                        most_recent_down == 0 || now.saturating_sub(most_recent_down) > wait_time;
                    if master_looks_sane
                        && slave.role_reported == crate::Role::Master
                        && !slave.flags.contains(InstanceFlags::PROMOTED)
                        && !slave.link.disconnected
                        && !slave.is_s_down()
                        && no_down_for_grace
                        && now.saturating_sub(slave.role_reported_time) > wait_time
                        && !slave.flags.contains(InstanceFlags::RECONF_SENT)
                    {
                        ios.push(FailoverIo::ReconfigureReplica {
                            master_name: master_name.to_string(),
                            replica_key: key.clone(),
                            ip: slave.addr.ip.clone(),
                            port: slave.addr.port,
                            new_ip: target_ip.clone(),
                            new_port: target_port,
                        });
                    }
                }
            }
            FailoverState::WaitStart => {
                // Needs the state, not just this master: the ballot is cast
                // through sentinel_vote_leader. Evaluated once the borrow ends.
                evaluate_election = true;
            }
            FailoverState::SelectSlave => {
                if let Some(key) = select_slave_at(master, now, ping_period, info_period) {
                    let mut ctx = FailoverContext::new();
                    advance_failover_state(
                        master,
                        FailoverEvent::SlaveSelected(key),
                        &mut ctx,
                        now,
                    );
                }
            }
            FailoverState::SendSlaveofNoone => {
                if let Some((replica_key, ip, port)) = master
                    .promoted_slave
                    .as_ref()
                    .map(|p| (p.name.clone(), p.addr.ip.clone(), p.addr.port))
                {
                    ios.push(FailoverIo::PromoteReplica {
                        master_name: master_name.to_string(),
                        replica_key,
                        ip,
                        port,
                    });
                }
            }
            FailoverState::WaitPromotion => {
                // Confirmed by `apply_replica_probe` when the promoted replica's
                // INFO reports role:master.
            }
            FailoverState::ReconfSlaves => {
                if let Some((promoted_key, new_ip, new_port)) = master
                    .promoted_slave
                    .as_ref()
                    .map(|p| (p.name.clone(), p.addr.ip.clone(), p.addr.port))
                {
                    let parallel = usize::try_from(master.parallel_syncs.max(1)).unwrap_or(1);
                    let in_flight = master
                        .slaves
                        .values()
                        .filter(|s| {
                            s.flags.contains(InstanceFlags::RECONF_SENT)
                                && !s.flags.contains(InstanceFlags::RECONF_DONE)
                        })
                        .count();
                    let mut budget = parallel.saturating_sub(in_flight);
                    let mut pending = false;
                    for (key, slave) in &master.slaves {
                        if *key == promoted_key
                            || slave.flags.contains(InstanceFlags::PROMOTED)
                            || slave.flags.contains(InstanceFlags::RECONF_DONE)
                        {
                            continue;
                        }
                        // Unreachable replicas do not hold the failover open;
                        // they are reconfigured by the `None` arm when they
                        // return, which is what upstream does after the
                        // failover-timeout grace as well.
                        if slave.is_s_down() || slave.link.disconnected {
                            continue;
                        }
                        pending = true;
                        if slave.flags.contains(InstanceFlags::RECONF_SENT) || budget == 0 {
                            continue;
                        }
                        budget -= 1;
                        ios.push(FailoverIo::ReconfigureReplica {
                            master_name: master_name.to_string(),
                            replica_key: key.clone(),
                            ip: slave.addr.ip.clone(),
                            port: slave.addr.port,
                            new_ip: new_ip.clone(),
                            new_port,
                        });
                    }
                    if !pending {
                        let mut ctx = FailoverContext::new();
                        advance_failover_state(
                            master,
                            FailoverEvent::ReconfigurationComplete,
                            &mut ctx,
                            now,
                        );
                    }
                }
            }
            FailoverState::UpdateConfig => {
                let mut ctx = FailoverContext::new();
                ctx.promoted_slave_key = master.promoted_slave.as_ref().map(|p| p.name.clone());
                finalize_ctx = Some(ctx);
            }
        }
    }
    if let Some(ctx) = finalize_ctx {
        finalize_failover(state, master_name, &ctx, now);
    }
    if let Some(new_epoch) = bump_epoch {
        state.current_epoch = new_epoch;
    }
    if evaluate_election {
        evaluate_wait_start(state, master_name, &my_runid, now);
    }
    ios
}

/// sentinelFailoverWaitStart: decide whether this sentinel leads the failover
/// it started. A forced failover (SENTINEL FAILOVER) skips the election.
/// Otherwise the ballot is cast here, as sentinelGetLeader does: for the
/// candidate the peers' replies already back in this epoch if there is one,
/// else for ourselves (a ballot already given to a peer that asked first
/// stands), and the election is won with a majority of the sentinels we know
/// about and at least `quorum` votes. A lost election is abandoned past the
/// election timeout; the backoff in should_start_failover spaces the retry.
fn evaluate_wait_start(state: &mut SentinelState, master_name: &str, my_runid: &str, now: u64) {
    let (forced, failover_epoch, quorum, sentinel_count, mut votes) = {
        let Some(master) = state.get_master(master_name) else {
            return;
        };
        if master.failover_state != FailoverState::WaitStart {
            return;
        }
        (
            master.flags.contains(InstanceFlags::FORCE_FAILOVER),
            master.failover_epoch,
            master.quorum,
            u32::try_from(master.sentinels.len())
                .unwrap_or(u32::MAX)
                .saturating_add(1),
            crate::consensus::peer_leader_votes(master),
        )
    };
    let won = forced || {
        let mut counts: std::collections::BTreeMap<&str, u32> = std::collections::BTreeMap::new();
        for vote in votes.iter().filter(|vote| vote.epoch == failover_epoch) {
            *counts.entry(vote.leader_runid.as_str()).or_insert(0) += 1;
        }
        let candidate = counts
            .iter()
            .max_by_key(|(_, count)| **count)
            .map_or_else(|| my_runid.to_string(), |(runid, _)| (*runid).to_string());
        let (ballot, ballot_epoch) = crate::commands::sentinel_vote_leader(
            state,
            master_name,
            failover_epoch,
            &candidate,
            now,
        );
        if let Some(leader) = ballot
            && ballot_epoch == failover_epoch
        {
            votes.push(crate::consensus::LeaderVote {
                voter_runid: my_runid.to_string(),
                leader_runid: leader,
                epoch: failover_epoch,
            });
        }
        crate::consensus::evaluate_leader_election(
            my_runid,
            failover_epoch,
            sentinel_count,
            quorum,
            &votes,
        )
        .is_winner
    };
    let Some(master) = state.masters.get_mut(master_name) else {
        return;
    };
    if won {
        master.failover_state = FailoverState::SelectSlave;
        master.failover_state_change_time = now;
    } else {
        let election_timeout = crate::ELECTION_TIMEOUT_MS.min(master.failover_timeout);
        if now.saturating_sub(master.failover_start_time) > election_timeout {
            let mut ctx = FailoverContext::new();
            advance_failover_state(
                master,
                FailoverEvent::Abort("election timeout".to_string()),
                &mut ctx,
                now,
            );
        }
    }
}

/// Fold the outcome of one [`FailoverIo`] back into the state machine. A
/// failed write changes nothing: the next tick re-issues it, which is the
/// retry loop upstream gets from `sentinelSendSlaveOf` being called again on
/// every `sentinelFailoverStateMachine` pass.
pub fn apply_failover_io_result(state: &mut SentinelState, io: &FailoverIo, ok: bool, now: u64) {
    if !ok {
        return;
    }
    match io {
        FailoverIo::PromoteReplica { master_name, .. } => {
            if let Some(master) = state.masters.get_mut(master_name)
                && master.failover_state == FailoverState::SendSlaveofNoone
            {
                let mut ctx = FailoverContext::new();
                advance_failover_state(master, FailoverEvent::SlaveofNoOneSent, &mut ctx, now);
            }
        }
        FailoverIo::ReconfigureReplica {
            master_name,
            replica_key,
            ..
        } => {
            if let Some(master) = state.masters.get_mut(master_name)
                && let Some(slave) = master.slaves.get_mut(replica_key)
            {
                slave.flags.insert(InstanceFlags::RECONF_SENT);
                slave.flags.remove(InstanceFlags::RECONF_INPROG);
            }
        }
    }
}

/// Fold one replica's PING + INFO probe into its instance: link liveness, the
/// INFO-derived role/offset/priority/master fields, subjective-down, and the
/// two confirmations the failover machine waits for -- the promoted replica
/// reporting `role:master` (WaitPromotion -> ReconfSlaves) and a reconfigured
/// replica reporting the expected `master_host`/`master_port` (`RECONF_DONE`,
/// or the end of a post-failover demotion when no failover is in flight).
/// `info == None` is a failed probe and marks the link disconnected.
pub fn apply_replica_probe(
    master: &mut SentinelRedisInstance,
    replica_key: &str,
    now: u64,
    info: Option<&str>,
) {
    let failover_state = master.failover_state;
    let master_addr = master.addr.clone();
    let promoted_addr = master.promoted_slave.as_ref().map(|p| p.addr.clone());
    let mut promotion_confirmed = false;
    {
        let Some(slave) = master.slaves.get_mut(replica_key) else {
            return;
        };
        crate::health::record_ping_sent(&mut slave.link, now);
        match info {
            Some(info) => {
                crate::health::record_pong(&mut slave.link, now);
                crate::health::record_reconnect(&mut slave.link, now);
                crate::health::record_info_response(slave, info, now);
            }
            None => crate::health::record_disconnect(&mut slave.link),
        }
        let health = crate::health::evaluate_instance_health(slave, now);
        crate::health::apply_health_result(slave, &health, now);

        if failover_state == FailoverState::WaitPromotion
            && slave.flags.contains(InstanceFlags::PROMOTED)
            && slave.role_reported == crate::Role::Master
        {
            promotion_confirmed = true;
        }

        if slave.flags.contains(InstanceFlags::RECONF_SENT)
            && slave.role_reported == crate::Role::Slave
        {
            let target = if failover_state == FailoverState::ReconfSlaves {
                promoted_addr.as_ref()
            } else {
                Some(&master_addr)
            };
            if let Some(target) = target
                && slave.slave_master_port == Some(target.port)
                && slave.slave_master_host.as_deref().is_some_and(|host| {
                    host.eq_ignore_ascii_case(&target.ip)
                        || host.eq_ignore_ascii_case(&target.hostname)
                })
            {
                slave.flags.remove(InstanceFlags::RECONF_SENT);
                slave.flags.remove(InstanceFlags::RECONF_INPROG);
                if failover_state == FailoverState::ReconfSlaves {
                    slave.flags.insert(InstanceFlags::RECONF_DONE);
                }
            }
        }
    }
    if promotion_confirmed {
        if let Some(snapshot) = master.slaves.get(replica_key).cloned() {
            master.promoted_slave = Some(Box::new(snapshot));
        }
        let mut ctx = FailoverContext::new();
        advance_failover_state(master, FailoverEvent::PromotionConfirmed, &mut ctx, now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SentinelAddr;

    fn make_master_with_slaves() -> SentinelRedisInstance {
        let addr = SentinelAddr::new("10.0.0.1", 6379);
        let mut master = SentinelRedisInstance::new_master("mymaster", addr, 2);

        let slave1_addr = SentinelAddr::new("10.0.0.10", 6379);
        let mut slave1 = SentinelRedisInstance::new_master("10.0.0.10:6379", slave1_addr, 0);
        slave1.flags = InstanceFlags::SLAVE;
        slave1.slave_priority = 100;
        slave1.slave_repl_offset = 1000;
        slave1.runid = Some("slave1_runid".to_string());

        let slave2_addr = SentinelAddr::new("10.0.0.11", 6379);
        let mut slave2 = SentinelRedisInstance::new_master("10.0.0.11:6379", slave2_addr, 0);
        slave2.flags = InstanceFlags::SLAVE;
        slave2.slave_priority = 100;
        slave2.slave_repl_offset = 2000;
        slave2.runid = Some("slave2_runid".to_string());

        master.slaves.insert("10.0.0.10:6379".to_string(), slave1);
        master.slaves.insert("10.0.0.11:6379".to_string(), slave2);

        master
    }

    fn replica_info(master_ip: &str, master_port: u16, offset: u64, runid: &str) -> String {
        format!(
            "# Replication\r\nrole:slave\r\nmaster_host:{master_ip}\r\nmaster_port:{master_port}\r\nmaster_link_status:up\r\nmaster_link_down_since_seconds:-1\r\nslave_repl_offset:{offset}\r\nslave_priority:100\r\nrun_id:{runid}\r\n"
        )
    }

    fn master_info(runid: &str) -> String {
        format!("# Replication\r\nrole:master\r\nconnected_slaves:0\r\nrun_id:{runid}\r\n")
    }

    /// A monitored master with two discovered replicas that have been probed
    /// (connected, fresh INFO), the shape fr-server builds before a failover.
    fn monitored_master_with_probed_replicas(state: &mut SentinelState, now: u64) {
        state.monitor("m1", "10.0.0.1", 6379, 1).unwrap();
        let master = state.get_master_mut("m1").unwrap();
        master.down_after_period = 1_000;
        master.failover_timeout = 10_000;
        for (ip, offset, runid) in [
            ("10.0.0.10", 100u64, "a".repeat(40)),
            ("10.0.0.11", 200u64, "b".repeat(40)),
        ] {
            let key = format!("{ip}:6379");
            let mut slave = SentinelRedisInstance::new_master(&key, SentinelAddr::new(ip, 6379), 0);
            slave.flags = InstanceFlags::SLAVE;
            slave.down_after_period = 1_000;
            slave.initialize_created_link_state(now);
            master.slaves.insert(key.clone(), slave);
            apply_replica_probe(
                master,
                &key,
                now,
                Some(&replica_info("10.0.0.1", 6379, offset, &runid)),
            );
        }
    }

    /// Run `failover_step` the way fr-server's tick does, answering each I/O
    /// request as a live instance would: `SLAVEOF NO ONE` makes the promoted
    /// replica report role:master, `SLAVEOF ip port` makes a replica report
    /// that master. Returns the promoted key once the machine finalizes.
    fn drive_to_finalize(state: &mut SentinelState, start: u64) -> String {
        let mut promoted = None;
        for tick in 0..40u64 {
            let now = start + tick * 500;
            for io in failover_step(state, "m1", now) {
                match &io {
                    FailoverIo::PromoteReplica {
                        replica_key, ip, ..
                    } => {
                        promoted = Some(replica_key.clone());
                        apply_failover_io_result(state, &io, true, now);
                        let master = state.get_master_mut("m1").unwrap();
                        assert_eq!(master.failover_state, FailoverState::WaitPromotion);
                        apply_replica_probe(
                            master,
                            replica_key,
                            now,
                            Some(&master_info(&format!("{:0>40}", ip.replace('.', "")))),
                        );
                        assert_eq!(master.failover_state, FailoverState::ReconfSlaves);
                    }
                    FailoverIo::ReconfigureReplica {
                        replica_key,
                        new_ip,
                        new_port,
                        ..
                    } => {
                        apply_failover_io_result(state, &io, true, now);
                        let master = state.get_master_mut("m1").unwrap();
                        assert!(
                            master.slaves[replica_key]
                                .flags
                                .contains(InstanceFlags::RECONF_SENT)
                        );
                        apply_replica_probe(
                            master,
                            replica_key,
                            now,
                            Some(&replica_info(new_ip, *new_port, 300, &"c".repeat(40))),
                        );
                    }
                }
            }
            let master = state.get_master("m1").unwrap();
            if promoted.is_some() && master.failover_state == FailoverState::None {
                return promoted.unwrap();
            }
        }
        panic!(
            "failover did not finalize; state={:?}",
            state.get_master("m1").unwrap().failover_state
        );
    }

    #[test]
    fn single_sentinel_failover_step_promotes_reconfigures_and_finalizes_after_o_down() {
        let mut state = SentinelState::new();
        let t0 = 1_000_000;
        monitored_master_with_probed_replicas(&mut state, t0);
        // Nothing happens while the master is healthy.
        assert!(failover_step(&mut state, "m1", t0 + 500).is_empty());
        assert_eq!(
            state.get_master("m1").unwrap().failover_state,
            FailoverState::None
        );

        // The detector marks the master down (quorum 1: the self vote suffices).
        {
            let master = state.get_master_mut("m1").unwrap();
            master.set_s_down(true, t0 + 1_000);
            master.set_o_down(true, t0 + 1_000);
        }
        let epoch_before = state.current_epoch;
        let promoted = drive_to_finalize(&mut state, t0 + 1_500);

        // The higher replication offset wins, exactly as select_slave_at ranks.
        assert_eq!(promoted, "10.0.0.11:6379");
        assert_eq!(state.current_epoch, epoch_before + 1);
        let master = state.get_master("m1").unwrap();
        assert_eq!(master.addr.ip, "10.0.0.11");
        assert_eq!(master.addr.port, 6379);
        assert_eq!(master.failover_state, FailoverState::None);
        assert!(!master.is_o_down() && !master.is_s_down());
        assert!(!master.flags.contains(InstanceFlags::FAILOVER_IN_PROGRESS));
        // The old master and the other replica are now this master's replicas.
        assert!(master.slaves.contains_key("10.0.0.1:6379"));
        assert!(master.slaves.contains_key("10.0.0.10:6379"));
        assert!(!master.slaves.contains_key("10.0.0.11:6379"));

        // The former master comes back still believing it is a master. It is
        // told to follow the new one only once the picture is stable: our
        // master answers INFO as a live master and the returned instance has
        // reported role:master for more than four publish periods
        // (sentinelMasterLooksSane plus the convert-to-slave grace).
        let returned = t0 + 60_000;
        {
            let master = state.get_master_mut("m1").unwrap();
            crate::health::record_info_response(master, &master_info(&"b".repeat(40)), returned);
            apply_replica_probe(
                master,
                "10.0.0.1:6379",
                returned,
                Some(&master_info(&"d".repeat(40))),
            );
        }
        assert!(
            failover_step(&mut state, "m1", returned + 1_000).is_empty(),
            "no demotion inside the grace period"
        );
        let now = returned + crate::PUBLISH_PERIOD_MS * 4 + 1_000;
        {
            let master = state.get_master_mut("m1").unwrap();
            crate::health::record_info_response(master, &master_info(&"b".repeat(40)), now);
            apply_replica_probe(
                master,
                "10.0.0.1:6379",
                now,
                Some(&master_info(&"d".repeat(40))),
            );
        }
        let ios = failover_step(&mut state, "m1", now);
        assert_eq!(
            ios,
            vec![FailoverIo::ReconfigureReplica {
                master_name: "m1".into(),
                replica_key: "10.0.0.1:6379".into(),
                ip: "10.0.0.1".into(),
                port: 6379,
                new_ip: "10.0.0.11".into(),
                new_port: 6379,
            }]
        );
        apply_failover_io_result(&mut state, &ios[0], true, now);
        // Once it reports following the new master the demotion is over and it
        // is not told again.
        {
            let master = state.get_master_mut("m1").unwrap();
            apply_replica_probe(
                master,
                "10.0.0.1:6379",
                now + 500,
                Some(&replica_info("10.0.0.11", 6379, 400, &"d".repeat(40))),
            );
        }
        assert!(failover_step(&mut state, "m1", now + 1_000).is_empty());
    }

    #[test]
    fn manual_failover_request_is_driven_to_finalize_without_o_down() {
        let mut state = SentinelState::new();
        let t0 = 5_000_000;
        monitored_master_with_probed_replicas(&mut state, t0);
        // What cmd_failover leaves behind for a forced failover.
        {
            let master = state.get_master_mut("m1").unwrap();
            master.flags.insert(InstanceFlags::FORCE_FAILOVER);
            master.flags.insert(InstanceFlags::FAILOVER_IN_PROGRESS);
            master.failover_state = FailoverState::WaitStart;
            master.failover_start_time = t0 + 400;
            master.failover_state_change_time = t0;
        }
        let promoted = drive_to_finalize(&mut state, t0);
        assert_eq!(promoted, "10.0.0.11:6379");
        assert_eq!(state.get_master("m1").unwrap().addr.ip, "10.0.0.11");
    }

    /// Without peer votes a quorum-2 failover starts, waits in WaitStart for
    /// an election it cannot win, and is abandoned at the election timeout
    /// without ever promoting anyone -- sentinelFailoverWaitStart's path.
    #[test]
    fn a_quorum_that_needs_peer_votes_waits_and_aborts_without_them() {
        let mut state = SentinelState::new();
        let t0 = 9_000_000;
        monitored_master_with_probed_replicas(&mut state, t0);
        {
            let master = state.get_master_mut("m1").unwrap();
            master.quorum = 2;
            master.set_s_down(true, t0);
            master.set_o_down(true, t0);
        }
        assert!(failover_step(&mut state, "m1", t0 + 500).is_empty());
        assert_eq!(
            state.get_master("m1").unwrap().failover_state,
            FailoverState::WaitStart
        );
        for tick in 1..20u64 {
            assert!(
                failover_step(&mut state, "m1", t0 + 500 + tick * 500).is_empty(),
                "no I/O may be requested while the election is open"
            );
            assert_ne!(
                state.get_master("m1").unwrap().failover_state,
                FailoverState::SelectSlave
            );
        }
        // Past ELECTION_TIMEOUT_MS (measured from the desynchronised start,
        // which may sit up to a second after the tick that started it) the
        // attempt is abandoned.
        assert!(
            failover_step(
                &mut state,
                "m1",
                t0 + 500 + crate::ELECTION_TIMEOUT_MS + 1_100
            )
            .is_empty()
        );
        let master = state.get_master("m1").unwrap();
        assert_eq!(master.failover_state, FailoverState::None);
        assert!(!master.flags.contains(InstanceFlags::FAILOVER_IN_PROGRESS));
    }

    fn add_peer(master: &mut SentinelRedisInstance, ip: &str, runid: &str, now: u64) {
        let key = format!("{ip}:26379");
        let mut peer = SentinelRedisInstance::new_master(&key, SentinelAddr::new(ip, 26379), 0);
        peer.flags = InstanceFlags::SENTINEL;
        peer.initialize_created_link_state(now);
        peer.runid = Some(runid.to_string());
        master.sentinels.insert(key, peer);
    }

    /// (frankenredis-rc-sentinel-peer-votes) Three sentinels, quorum 2: this
    /// one sees the master S_DOWN, asks its two peers, their down answers make
    /// O_DOWN, the failover starts in a new epoch, the peers' ballots for us
    /// win the election, and the machine proceeds to promote a replica.
    #[test]
    fn peer_votes_reach_o_down_and_elect_this_sentinel_leader() {
        use crate::consensus::{PeerReply, apply_peer_reply, peer_asks};
        let mut state = SentinelState::new();
        let myid = state.myid_hex();
        let t0 = 11_000_000;
        monitored_master_with_probed_replicas(&mut state, t0);
        {
            let master = state.get_master_mut("m1").unwrap();
            master.quorum = 2;
            add_peer(master, "10.0.0.20", &"e".repeat(40), t0);
            add_peer(master, "10.0.0.21", &"f".repeat(40), t0);
        }
        // Nothing is asked while the master looks healthy.
        assert!(peer_asks(&state, "m1", t0 + 1_000).is_empty());

        state
            .get_master_mut("m1")
            .unwrap()
            .set_s_down(true, t0 + 1_000);
        let asks = peer_asks(&state, "m1", t0 + 1_500);
        assert_eq!(asks.len(), 2);
        assert!(asks.iter().all(|ask| ask.runid_arg == "*"), "{asks:?}");
        assert!(asks.iter().all(|ask| ask.master_port == 6379));
        for ask in &asks {
            apply_peer_reply(
                &mut state,
                "m1",
                &ask.sentinel_key,
                Some(PeerReply {
                    is_down: true,
                    leader: None,
                    leader_epoch: 0,
                }),
                t0 + 1_600,
            );
        }
        // The peers' answers plus our own view reach the quorum.
        {
            let master = state.get_master_mut("m1").unwrap();
            let votes = crate::consensus::peer_o_down_votes(master);
            let result = crate::consensus::evaluate_o_down(master, &votes, t0 + 1_700);
            assert!(result.should_mark_o_down, "{result:?}");
            crate::consensus::apply_o_down_result(master, &result, t0 + 1_700);
            assert!(master.is_o_down());
        }
        // The failover starts in a fresh epoch and asks for votes by run id.
        let epoch_before = state.current_epoch;
        assert!(failover_step(&mut state, "m1", t0 + 2_000).is_empty());
        let failover_epoch = state.get_master("m1").unwrap().failover_epoch;
        assert_eq!(failover_epoch, epoch_before + 1);
        assert_eq!(
            state.get_master("m1").unwrap().failover_state,
            FailoverState::WaitStart
        );
        let asks = peer_asks(&state, "m1", t0 + 2_100);
        assert_eq!(asks.len(), 2, "forced asks during a failover");
        assert!(asks.iter().all(|ask| ask.runid_arg == myid));
        for ask in &asks {
            apply_peer_reply(
                &mut state,
                "m1",
                &ask.sentinel_key,
                Some(PeerReply {
                    is_down: true,
                    leader: Some(myid.clone()),
                    leader_epoch: failover_epoch,
                }),
                t0 + 2_200,
            );
        }
        // Elected: the machine moves on and eventually asks to promote.
        let mut promoted = false;
        for tick in 0..6u64 {
            for io in failover_step(&mut state, "m1", t0 + 2_500 + tick * 500) {
                if matches!(io, FailoverIo::PromoteReplica { .. }) {
                    promoted = true;
                }
            }
        }
        assert!(promoted, "peer votes must let the failover proceed");
    }

    /// A losing election: the peers voted for someone else in this epoch, so
    /// this sentinel never promotes and abandons the attempt at the timeout.
    #[test]
    fn peer_votes_for_another_sentinel_stop_this_one_from_promoting() {
        use crate::consensus::{PeerReply, apply_peer_reply, peer_asks};
        let mut state = SentinelState::new();
        let t0 = 13_000_000;
        monitored_master_with_probed_replicas(&mut state, t0);
        {
            let master = state.get_master_mut("m1").unwrap();
            master.quorum = 2;
            add_peer(master, "10.0.0.20", &"e".repeat(40), t0);
            add_peer(master, "10.0.0.21", &"f".repeat(40), t0);
            master.set_s_down(true, t0);
            master.set_o_down(true, t0);
        }
        assert!(failover_step(&mut state, "m1", t0 + 500).is_empty());
        let failover_epoch = state.get_master("m1").unwrap().failover_epoch;
        for ask in peer_asks(&state, "m1", t0 + 600) {
            apply_peer_reply(
                &mut state,
                "m1",
                &ask.sentinel_key,
                Some(PeerReply {
                    is_down: true,
                    leader: Some("e".repeat(40)),
                    leader_epoch: failover_epoch,
                }),
                t0 + 700,
            );
        }
        for tick in 0..30u64 {
            let ios = failover_step(&mut state, "m1", t0 + 1_000 + tick * 500);
            assert!(
                ios.is_empty(),
                "a losing candidate must not promote: {ios:?}"
            );
        }
        assert_eq!(
            state.get_master("m1").unwrap().failover_state,
            FailoverState::None,
            "abandoned at the election timeout"
        );
    }

    /// (frankenredis-rc-sentinel-peer-votes) sentinelGetLeader's tie-break: a
    /// candidate that starts its own failover after its peers' replies already
    /// showed a vote for another sentinel in that epoch backs that sentinel
    /// instead of itself, so two sentinels reaching O_DOWN in the same epoch
    /// do not split the vote.
    #[test]
    fn a_candidate_backs_the_leader_its_peers_already_voted_for() {
        use crate::consensus::{PeerReply, apply_peer_reply};
        let mut state = SentinelState::new();
        let t0 = 15_000_000;
        monitored_master_with_probed_replicas(&mut state, t0);
        let claimed = state.current_epoch + 1;
        {
            let master = state.get_master_mut("m1").unwrap();
            master.quorum = 2;
            add_peer(master, "10.0.0.20", &"e".repeat(40), t0);
            add_peer(master, "10.0.0.21", &"f".repeat(40), t0);
            master.set_s_down(true, t0);
        }
        // Peer f's answer to our plain down question already carries its vote
        // for e in the epoch e claimed.
        apply_peer_reply(
            &mut state,
            "m1",
            "10.0.0.21:26379",
            Some(PeerReply {
                is_down: true,
                leader: Some("e".repeat(40)),
                leader_epoch: claimed,
            }),
            t0 + 100,
        );
        state
            .get_master_mut("m1")
            .unwrap()
            .set_o_down(true, t0 + 200);
        assert!(failover_step(&mut state, "m1", t0 + 300).is_empty());
        let master = state.get_master("m1").unwrap();
        assert_eq!(master.failover_epoch, claimed);
        assert_eq!(master.leader.as_deref(), Some("e".repeat(40).as_str()));
        assert_eq!(master.leader_epoch, claimed);
        assert_eq!(
            master.failover_state,
            FailoverState::WaitStart,
            "e has the majority, so this sentinel must not promote"
        );
    }

    /// The bug the three-sentinel e2e caught: a follower probing the replica a
    /// peer had just promoted saw role:master and, with no failover of its own
    /// in flight, told it to follow the dead master again. The convert-to-slave
    /// path needs the follower's own master to look sane (alive, fresh INFO)
    /// and the instance to have reported master for 4 x publish period.
    #[test]
    fn a_replica_promoted_by_a_peer_is_not_demoted_while_the_master_is_down() {
        let mut state = SentinelState::new();
        let t0 = 17_000_000;
        monitored_master_with_probed_replicas(&mut state, t0);
        {
            let master = state.get_master_mut("m1").unwrap();
            master.quorum = 2;
            master.set_s_down(true, t0 + 1_000);
            // The peer leader promoted 10.0.0.11; our next probe sees it.
            apply_replica_probe(
                master,
                "10.0.0.11:6379",
                t0 + 2_000,
                Some(&master_info(&"b".repeat(40))),
            );
        }
        for tick in 0..30u64 {
            let ios = failover_step(&mut state, "m1", t0 + 2_000 + tick * 500);
            assert!(
                ios.is_empty(),
                "a sentinel whose master is down must not demote the new one: {ios:?}"
            );
        }
        // The old master back and sane, and the instance still claiming to be
        // a master long past the grace: now it is converted, as upstream does.
        let back = t0 + 20_000;
        {
            let master = state.get_master_mut("m1").unwrap();
            master.flags.remove(InstanceFlags::S_DOWN);
            crate::health::record_info_response(master, &master_info(&"a".repeat(40)), back);
        }
        let ios = failover_step(&mut state, "m1", back);
        assert!(
            matches!(
                ios.as_slice(),
                [FailoverIo::ReconfigureReplica { replica_key, .. }] if replica_key == "10.0.0.11:6379"
            ),
            "{ios:?}"
        );
    }

    #[test]
    fn failed_promotion_write_is_retried_on_the_next_tick() {
        let mut state = SentinelState::new();
        let t0 = 3_000_000;
        monitored_master_with_probed_replicas(&mut state, t0);
        {
            let master = state.get_master_mut("m1").unwrap();
            master.set_s_down(true, t0);
            master.set_o_down(true, t0);
        }
        let mut first = None;
        for tick in 0..6u64 {
            let ios = failover_step(&mut state, "m1", t0 + 500 + tick * 500);
            if let Some(io) = ios.into_iter().next() {
                first = Some(io);
                break;
            }
        }
        let io = first.expect("a promotion request");
        assert!(matches!(io, FailoverIo::PromoteReplica { .. }));
        apply_failover_io_result(&mut state, &io, false, t0 + 4_000);
        assert_eq!(
            state.get_master("m1").unwrap().failover_state,
            FailoverState::SendSlaveofNoone
        );
        let again = failover_step(&mut state, "m1", t0 + 4_500);
        assert_eq!(again, vec![io]);
    }

    #[test]
    fn select_slave_picks_highest_offset() {
        let master = make_master_with_slaves();
        let selected = select_slave(&master).unwrap();
        assert_eq!(selected, "10.0.0.11:6379");
    }

    #[test]
    fn select_slave_prefers_lower_priority() {
        let mut master = make_master_with_slaves();
        if let Some(slave) = master.slaves.get_mut("10.0.0.10:6379") {
            slave.slave_priority = 50;
        }
        let selected = select_slave(&master).unwrap();
        assert_eq!(selected, "10.0.0.10:6379");
    }

    #[test]
    fn select_slave_breaks_runid_ties_case_insensitively() {
        let mut master = make_master_with_slaves();
        let slave1 = master.slaves.get_mut("10.0.0.10:6379").unwrap();
        slave1.slave_repl_offset = 1000;
        slave1.runid = Some("a-runid".to_string());

        let slave2 = master.slaves.get_mut("10.0.0.11:6379").unwrap();
        slave2.slave_repl_offset = 1000;
        slave2.runid = Some("B-runid".to_string());

        let selected = select_slave(&master).unwrap();
        assert_eq!(selected, "10.0.0.10:6379");
    }

    #[test]
    fn select_slave_preserves_first_iteration_winner_for_exact_score_ties() {
        let mut master = make_master_with_slaves();
        for slave in master.slaves.values_mut() {
            slave.slave_repl_offset = 1_000;
            slave.runid = Some("same-runid".to_owned());
        }
        let first = master.slaves.keys().next().unwrap().clone();

        assert_eq!(select_slave(&master), Some(first));
    }

    #[test]
    fn select_slave_excludes_disconnected() {
        let mut master = make_master_with_slaves();
        if let Some(slave) = master.slaves.get_mut("10.0.0.11:6379") {
            slave.link.disconnected = true;
        }
        let selected = select_slave(&master).unwrap();
        assert_eq!(selected, "10.0.0.10:6379");
    }

    #[test]
    fn select_slave_excludes_zero_priority() {
        let mut master = make_master_with_slaves();
        master
            .slaves
            .get_mut("10.0.0.11:6379")
            .unwrap()
            .slave_priority = 0;
        let selected = select_slave(&master).unwrap();
        assert_eq!(selected, "10.0.0.10:6379");
    }

    #[test]
    fn select_slave_excludes_s_down() {
        let mut master = make_master_with_slaves();
        master
            .slaves
            .get_mut("10.0.0.11:6379")
            .unwrap()
            .flags
            .insert(InstanceFlags::S_DOWN);
        let selected = select_slave(&master).unwrap();
        assert_eq!(selected, "10.0.0.10:6379");
    }

    #[test]
    fn select_slave_at_excludes_stale_upstream_candidates() {
        let mut master = make_master_with_slaves();
        let now = 100_000;
        for slave in master.slaves.values_mut() {
            slave.link.last_avail_time = now;
            slave.info_refresh = 0;
        }

        assert_eq!(select_slave_at(&master, now, 1_000, 10_000), None);

        master
            .slaves
            .get_mut("10.0.0.10:6379")
            .unwrap()
            .info_refresh = now;
        assert_eq!(
            select_slave_at(&master, now, 1_000, 10_000),
            Some("10.0.0.10:6379".into())
        );

        let stale_master_link_time = master.down_after_period.saturating_mul(10) + 1;
        master
            .slaves
            .get_mut("10.0.0.10:6379")
            .unwrap()
            .master_link_down_time = stale_master_link_time;
        assert_eq!(select_slave_at(&master, now, 1_000, 10_000), None);
    }

    #[test]
    fn failover_state_progression() {
        let addr = SentinelAddr::new("10.0.0.1", 6379);
        let mut master = SentinelRedisInstance::new_master("mymaster", addr, 2);
        let slave =
            SentinelRedisInstance::new_master("replica-1", SentinelAddr::new("10.0.0.2", 6380), 0);
        master.slaves.insert("replica-1".to_string(), slave);
        let mut ctx = FailoverContext::new();

        let state =
            advance_failover_state(&mut master, FailoverEvent::StartFailover, &mut ctx, 1000);
        assert_eq!(state, FailoverState::WaitStart);

        let state = advance_failover_state(
            &mut master,
            FailoverEvent::SlaveSelected("10.0.0.10:6379".to_string()),
            &mut ctx,
            2000,
        );
        assert_eq!(state, FailoverState::WaitStart);

        let state = advance_failover_state(
            &mut master,
            FailoverEvent::SlaveSelected("replica-1".to_string()),
            &mut ctx,
            2500,
        );
        assert_eq!(state, FailoverState::SendSlaveofNoone);
        assert_eq!(ctx.promoted_slave_key, Some("replica-1".to_string()));
        assert!(ctx.slaves_to_reconfig.is_empty());
        assert!(master.promoted_slave.is_some());
        assert!(
            master
                .slaves
                .get("replica-1")
                .is_some_and(|slave| slave.flags.contains(InstanceFlags::PROMOTED))
        );

        let state =
            advance_failover_state(&mut master, FailoverEvent::SlaveofNoOneSent, &mut ctx, 3000);
        assert_eq!(state, FailoverState::WaitPromotion);

        let state = advance_failover_state(
            &mut master,
            FailoverEvent::PromotionConfirmed,
            &mut ctx,
            4000,
        );
        assert_eq!(state, FailoverState::ReconfSlaves);
    }

    #[test]
    fn failover_selection_skips_s_down_reconfiguration_targets_like_upstream() {
        let addr = SentinelAddr::new("10.0.0.1", 6379);
        let mut master = SentinelRedisInstance::new_master("mymaster", addr, 2);
        let promoted =
            SentinelRedisInstance::new_master("promoted", SentinelAddr::new("10.0.0.2", 6380), 0);
        let healthy =
            SentinelRedisInstance::new_master("healthy", SentinelAddr::new("10.0.0.3", 6381), 0);
        let mut s_down =
            SentinelRedisInstance::new_master("s-down", SentinelAddr::new("10.0.0.4", 6382), 0);
        s_down.flags.insert(InstanceFlags::S_DOWN);
        master.slaves.insert("promoted".to_string(), promoted);
        master.slaves.insert("healthy".to_string(), healthy);
        master.slaves.insert("s-down".to_string(), s_down);
        master.failover_state = FailoverState::SelectSlave;
        let mut ctx = FailoverContext::new();

        let state = advance_failover_state(
            &mut master,
            FailoverEvent::SlaveSelected("promoted".to_string()),
            &mut ctx,
            2500,
        );

        assert_eq!(state, FailoverState::SendSlaveofNoone);
        assert_eq!(ctx.slaves_to_reconfig, vec!["healthy".to_string()]);
        assert!(!is_reconfiguration_complete(&ctx));
        track_slave_reconfiguration(&mut ctx, "healthy", ReconfigStatus::Done);
        assert!(is_reconfiguration_complete(&ctx));
    }

    #[test]
    fn failover_timeout_detection() {
        let addr = SentinelAddr::new("10.0.0.1", 6379);
        let mut master = SentinelRedisInstance::new_master("mymaster", addr, 2);
        master.failover_state = FailoverState::WaitStart;
        master.failover_start_time = 0;
        master.failover_timeout = 180000;

        assert!(!check_failover_timeout(&master, 100000));
        assert!(check_failover_timeout(&master, 200000));
    }

    #[test]
    fn failover_timeout_uses_state_change_time_after_wait_start() {
        let mut master =
            SentinelRedisInstance::new_master("mymaster", SentinelAddr::new("10.0.0.1", 6379), 2);
        master.failover_state = FailoverState::WaitPromotion;
        master.failover_start_time = 1_000;
        master.failover_state_change_time = 200_000;
        master.failover_timeout = 30_000;

        assert!(!check_failover_timeout(&master, 229_999));
        assert!(!check_failover_timeout(&master, 230_000));
        assert!(check_failover_timeout(&master, 230_001));
    }

    #[test]
    fn failover_abort_resets_state() {
        let addr = SentinelAddr::new("10.0.0.1", 6379);
        let mut master = SentinelRedisInstance::new_master("mymaster", addr, 2);
        master.failover_state = FailoverState::SelectSlave;
        master.flags.insert(InstanceFlags::FAILOVER_IN_PROGRESS);
        master.flags.insert(InstanceFlags::FORCE_FAILOVER);
        let mut promoted =
            SentinelRedisInstance::new_master("replica-1", SentinelAddr::new("10.0.0.2", 6380), 0);
        promoted.flags.insert(InstanceFlags::PROMOTED);
        master.promoted_slave = Some(Box::new(promoted));
        let mut stored =
            SentinelRedisInstance::new_master("replica-1", SentinelAddr::new("10.0.0.2", 6380), 0);
        stored.flags.insert(InstanceFlags::PROMOTED);
        master.slaves.insert("replica-1".to_string(), stored);
        let mut ctx = FailoverContext::new();

        let state = advance_failover_state(
            &mut master,
            FailoverEvent::Abort("test abort".to_string()),
            &mut ctx,
            1000,
        );
        assert_eq!(state, FailoverState::None);
        assert!(!master.flags.contains(InstanceFlags::FAILOVER_IN_PROGRESS));
        assert!(!master.flags.contains(InstanceFlags::FORCE_FAILOVER));
        assert!(master.promoted_slave.is_none());
        assert!(
            !master
                .slaves
                .get("replica-1")
                .is_some_and(|slave| slave.flags.contains(InstanceFlags::PROMOTED))
        );
    }

    #[test]
    fn failover_timeout_clears_forced_and_promoted_state() {
        let addr = SentinelAddr::new("10.0.0.1", 6379);
        let mut master = SentinelRedisInstance::new_master("mymaster", addr, 2);
        master.failover_state = FailoverState::WaitPromotion;
        master.flags.insert(InstanceFlags::FAILOVER_IN_PROGRESS);
        master.flags.insert(InstanceFlags::FORCE_FAILOVER);
        let mut promoted =
            SentinelRedisInstance::new_master("replica-1", SentinelAddr::new("10.0.0.2", 6380), 0);
        promoted.flags.insert(InstanceFlags::PROMOTED);
        master.promoted_slave = Some(Box::new(promoted));
        let mut stored =
            SentinelRedisInstance::new_master("replica-1", SentinelAddr::new("10.0.0.2", 6380), 0);
        stored.flags.insert(InstanceFlags::PROMOTED);
        master.slaves.insert("replica-1".to_string(), stored);
        let mut ctx = FailoverContext::new();

        let state = advance_failover_state(&mut master, FailoverEvent::Timeout, &mut ctx, 2000);

        assert_eq!(state, FailoverState::None);
        assert_eq!(master.failover_state, FailoverState::None);
        assert_eq!(master.failover_state_change_time, 2000);
        assert!(!master.flags.contains(InstanceFlags::FAILOVER_IN_PROGRESS));
        assert!(!master.flags.contains(InstanceFlags::FORCE_FAILOVER));
        assert!(master.promoted_slave.is_none());
        assert!(
            !master
                .slaves
                .get("replica-1")
                .is_some_and(|slave| slave.flags.contains(InstanceFlags::PROMOTED))
        );
    }

    #[test]
    fn generate_slaveof_commands() {
        let cmd = generate_slaveof_command("10.0.0.1", 6379);
        assert_eq!(cmd.len(), 3);
        assert_eq!(cmd[0], b"SLAVEOF");
        assert_eq!(cmd[1], b"10.0.0.1");
        assert_eq!(cmd[2], b"6379");

        let cmd = generate_slaveof_no_one();
        assert_eq!(cmd.len(), 3);
        assert_eq!(cmd[0], b"SLAVEOF");
        assert_eq!(cmd[1], b"NO");
        assert_eq!(cmd[2], b"ONE");
    }

    #[test]
    fn reconfiguration_tracking() {
        let mut ctx = FailoverContext::new();
        ctx.slaves_to_reconfig = vec!["10.0.0.10:6379".to_string(), "10.0.0.11:6379".to_string()];

        assert!(!is_reconfiguration_complete(&ctx));

        track_slave_reconfiguration(&mut ctx, "10.0.0.10:6379", ReconfigStatus::Done);
        assert!(!is_reconfiguration_complete(&ctx));

        track_slave_reconfiguration(&mut ctx, "10.0.0.11:6379", ReconfigStatus::Done);
        assert!(is_reconfiguration_complete(&ctx));
    }

    #[test]
    fn finalize_failover_readds_old_master_as_replica_like_upstream() {
        let mut state = SentinelState::new();
        state.current_epoch = 9;
        let mut master =
            SentinelRedisInstance::new_master("mymaster", SentinelAddr::new("10.0.0.1", 6379), 2);
        master.flags.insert(InstanceFlags::FAILOVER_IN_PROGRESS);
        master.flags.insert(InstanceFlags::S_DOWN);
        master.flags.insert(InstanceFlags::O_DOWN);
        master.failover_epoch = 7;
        master.failover_state = FailoverState::UpdateConfig;
        master.failover_state_change_time = 10_000;
        master.failover_start_time = 9_000;
        master.leader = Some("leader".to_string());
        master.down_after_period = 12_345;

        let mut promoted =
            SentinelRedisInstance::new_master("promoted", SentinelAddr::new("10.0.0.2", 6380), 0);
        promoted.flags = InstanceFlags::SLAVE.union(InstanceFlags::PROMOTED);
        promoted.runid = Some("promoted-runid".to_string());
        master.slaves.insert("promoted".to_string(), promoted);

        let mut existing =
            SentinelRedisInstance::new_master("existing", SentinelAddr::new("10.0.0.3", 6381), 0);
        existing.flags = InstanceFlags::SLAVE;
        master.slaves.insert("existing".to_string(), existing);

        let mut sentinel =
            SentinelRedisInstance::new_master("sentinel", SentinelAddr::new("10.0.0.4", 26379), 0);
        sentinel.flags = InstanceFlags::SENTINEL;
        master.sentinels.insert("sentinel".to_string(), sentinel);

        state.masters.insert("mymaster".to_string(), master);
        let ctx = FailoverContext {
            promoted_slave_key: Some("promoted".to_string()),
            slaves_to_reconfig: vec!["existing".to_string()],
            slaves_reconfigured: vec!["existing".to_string()],
        };

        finalize_failover(&mut state, "mymaster", &ctx, 50_000);

        let master = state.get_master("mymaster").expect("master retained");
        assert_eq!(master.addr, SentinelAddr::new("10.0.0.2", 6380));
        assert_eq!(master.runid, None);
        assert_eq!(master.config_epoch, 7);
        assert_eq!(master.flags, InstanceFlags::MASTER);
        assert_eq!(master.failover_state, FailoverState::None);
        assert_eq!(master.failover_state_change_time, 0);
        assert_eq!(master.failover_start_time, 0);
        assert!(master.promoted_slave.is_none());
        assert_eq!(master.leader, None);
        assert_eq!(master.s_down_since_time, 0);
        assert_eq!(master.o_down_since_time, 0);
        assert_eq!(master.role_reported, crate::Role::Master);
        assert_eq!(master.role_reported_time, 50_000);
        assert_eq!(master.link.act_ping_time, 50_000);
        assert_eq!(master.link.last_avail_time, 50_000);
        assert_eq!(master.sentinels.len(), 1);

        assert!(master.slaves.contains_key("10.0.0.1:6379"));
        assert!(master.slaves.contains_key("10.0.0.3:6381"));
        assert!(!master.slaves.contains_key("10.0.0.2:6380"));
        for slave in master.slaves.values() {
            assert_eq!(slave.flags, InstanceFlags::SLAVE);
            assert_eq!(slave.down_after_period, 12_345);
            assert_eq!(slave.role_reported, crate::Role::Slave);
            assert_eq!(slave.role_reported_time, 50_000);
        }
    }

    #[test]
    fn should_start_failover_checks() {
        let addr = SentinelAddr::new("10.0.0.1", 6379);
        let master = SentinelRedisInstance::new_master("mymaster", addr, 2);
        let now = 500_000;

        assert!(!should_start_failover(&master, true, now));

        let mut o_down_master =
            SentinelRedisInstance::new_master("mymaster", SentinelAddr::new("10.0.0.1", 6379), 2);
        o_down_master.flags.insert(InstanceFlags::O_DOWN);
        assert!(should_start_failover(&o_down_master, true, now));
        assert!(should_start_failover(&o_down_master, false, now));

        o_down_master.flags.insert(InstanceFlags::FORCE_FAILOVER);
        assert!(should_start_failover(&o_down_master, false, now));
    }

    #[test]
    fn should_start_failover_honors_recent_attempt_cooldown() {
        let mut master =
            SentinelRedisInstance::new_master("mymaster", SentinelAddr::new("10.0.0.1", 6379), 2);
        master.flags.insert(InstanceFlags::O_DOWN);
        master.failover_start_time = 1_000;
        master.failover_timeout = 10_000;

        assert!(!should_start_failover(&master, true, 20_999));
        assert!(should_start_failover(&master, true, 21_000));
    }

    #[test]
    fn should_start_failover_rejects_active_failover_flag() {
        let mut master =
            SentinelRedisInstance::new_master("mymaster", SentinelAddr::new("10.0.0.1", 6379), 2);
        master.flags.insert(InstanceFlags::O_DOWN);
        master.flags.insert(InstanceFlags::FAILOVER_IN_PROGRESS);

        assert!(!should_start_failover(&master, true, 500_000));
    }
}
