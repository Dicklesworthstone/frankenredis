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

fn sentinel_slave_key(addr: &crate::SentinelAddr) -> String {
    format!("{}:{}", addr.hostname, addr.port)
}

fn reset_slave_instance(
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
