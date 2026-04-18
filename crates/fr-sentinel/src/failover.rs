#![forbid(unsafe_code)]

use crate::{
    FailoverState, InstanceFlags, LinkStatus, SentinelRedisInstance, SentinelState,
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
        Self {
            key: key.to_string(),
            priority: slave.slave_priority,
            repl_offset: slave.slave_repl_offset,
            runid: slave.runid.clone(),
            is_connected: !slave.link.disconnected,
            master_link_up: slave.slave_master_link_status == LinkStatus::Up,
        }
    }
}

pub fn select_slave(master: &SentinelRedisInstance) -> Option<String> {
    let mut candidates: Vec<SlaveScore> = master
        .slaves
        .iter()
        .filter(|(_, slave)| is_slave_eligible(slave))
        .map(|(key, slave)| SlaveScore::from_instance(key, slave))
        .collect();

    if candidates.is_empty() {
        return None;
    }

    candidates.sort_by(compare_slaves);

    Some(candidates[0].key.clone())
}

fn is_slave_eligible(slave: &SentinelRedisInstance) -> bool {
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
    true
}

fn compare_slaves(a: &SlaveScore, b: &SlaveScore) -> std::cmp::Ordering {
    if a.priority != b.priority {
        return a.priority.cmp(&b.priority);
    }

    if a.repl_offset != b.repl_offset {
        return b.repl_offset.cmp(&a.repl_offset);
    }

    match (&a.runid, &b.runid) {
        (Some(ra), Some(rb)) => ra.cmp(rb),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    }
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

        (FailoverState::WaitStart, FailoverEvent::SlaveSelected(key)) => {
            ctx.promoted_slave_key = Some(key.clone());
            ctx.slaves_to_reconfig = master
                .slaves
                .keys()
                .filter(|k| *k != &key)
                .cloned()
                .collect();
            master.failover_state_change_time = now;
            FailoverState::SelectSlave
        }

        (FailoverState::SelectSlave, FailoverEvent::SlaveofNoOneSent) => {
            master.failover_state_change_time = now;
            FailoverState::SendSlaveofNoone
        }

        (FailoverState::SendSlaveofNoone, FailoverEvent::PromotionConfirmed) => {
            master.failover_state_change_time = now;
            FailoverState::WaitPromotion
        }

        (FailoverState::WaitPromotion, FailoverEvent::ReconfigurationComplete) => {
            master.failover_state_change_time = now;
            FailoverState::ReconfSlaves
        }

        (FailoverState::ReconfSlaves, FailoverEvent::ReconfigurationComplete) => {
            master.failover_state_change_time = now;
            FailoverState::UpdateConfig
        }

        (_, FailoverEvent::Timeout) => {
            master.failover_state_change_time = now;
            master.flags.remove(InstanceFlags::FAILOVER_IN_PROGRESS);
            FailoverState::None
        }

        (_, FailoverEvent::Abort(_)) => {
            master.failover_state_change_time = now;
            master.flags.remove(InstanceFlags::FAILOVER_IN_PROGRESS);
            FailoverState::None
        }

        _ => current,
    };

    master.failover_state = next;
    next
}

pub fn check_failover_timeout(
    master: &SentinelRedisInstance,
    now: u64,
) -> bool {
    if master.failover_state == FailoverState::None {
        return false;
    }
    now.saturating_sub(master.failover_start_time) > master.failover_timeout
}

pub fn should_start_failover(
    master: &SentinelRedisInstance,
    is_leader: bool,
) -> bool {
    if !master.is_o_down() {
        return false;
    }
    if master.failover_state != FailoverState::None {
        return false;
    }
    if !is_leader && !master.flags.contains(InstanceFlags::FORCE_FAILOVER) {
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
    if let Some(master) = state.get_master_mut(master_name) {
        if let Some(ref promoted_key) = ctx.promoted_slave_key {
            if let Some(promoted) = master.slaves.remove(promoted_key) {
                master.addr = promoted.addr.clone();
                master.runid = promoted.runid.clone();
                master.config_epoch = current_epoch;
                master.failover_state = FailoverState::None;
                master.failover_state_change_time = now;
                master.flags.remove(InstanceFlags::FAILOVER_IN_PROGRESS);
                master.flags.remove(InstanceFlags::O_DOWN);
                master.flags.remove(InstanceFlags::S_DOWN);
            }
        }
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
        master.slaves.get_mut("10.0.0.11:6379").unwrap().slave_priority = 0;
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
    fn failover_state_progression() {
        let addr = SentinelAddr::new("10.0.0.1", 6379);
        let mut master = SentinelRedisInstance::new_master("mymaster", addr, 2);
        let mut ctx = FailoverContext::new();

        let state = advance_failover_state(
            &mut master,
            FailoverEvent::StartFailover,
            &mut ctx,
            1000,
        );
        assert_eq!(state, FailoverState::WaitStart);

        let state = advance_failover_state(
            &mut master,
            FailoverEvent::SlaveSelected("10.0.0.10:6379".to_string()),
            &mut ctx,
            2000,
        );
        assert_eq!(state, FailoverState::SelectSlave);
        assert_eq!(ctx.promoted_slave_key, Some("10.0.0.10:6379".to_string()));

        let state = advance_failover_state(
            &mut master,
            FailoverEvent::SlaveofNoOneSent,
            &mut ctx,
            3000,
        );
        assert_eq!(state, FailoverState::SendSlaveofNoone);

        let state = advance_failover_state(
            &mut master,
            FailoverEvent::PromotionConfirmed,
            &mut ctx,
            4000,
        );
        assert_eq!(state, FailoverState::WaitPromotion);
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
    fn failover_abort_resets_state() {
        let addr = SentinelAddr::new("10.0.0.1", 6379);
        let mut master = SentinelRedisInstance::new_master("mymaster", addr, 2);
        master.failover_state = FailoverState::SelectSlave;
        master.flags.insert(InstanceFlags::FAILOVER_IN_PROGRESS);
        let mut ctx = FailoverContext::new();

        let state = advance_failover_state(
            &mut master,
            FailoverEvent::Abort("test abort".to_string()),
            &mut ctx,
            1000,
        );
        assert_eq!(state, FailoverState::None);
        assert!(!master.flags.contains(InstanceFlags::FAILOVER_IN_PROGRESS));
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
        ctx.slaves_to_reconfig = vec![
            "10.0.0.10:6379".to_string(),
            "10.0.0.11:6379".to_string(),
        ];

        assert!(!is_reconfiguration_complete(&ctx));

        track_slave_reconfiguration(&mut ctx, "10.0.0.10:6379", ReconfigStatus::Done);
        assert!(!is_reconfiguration_complete(&ctx));

        track_slave_reconfiguration(&mut ctx, "10.0.0.11:6379", ReconfigStatus::Done);
        assert!(is_reconfiguration_complete(&ctx));
    }

    #[test]
    fn should_start_failover_checks() {
        let addr = SentinelAddr::new("10.0.0.1", 6379);
        let master = SentinelRedisInstance::new_master("mymaster", addr, 2);

        assert!(!should_start_failover(&master, true));

        let mut o_down_master = SentinelRedisInstance::new_master(
            "mymaster",
            SentinelAddr::new("10.0.0.1", 6379),
            2,
        );
        o_down_master.flags.insert(InstanceFlags::O_DOWN);
        assert!(should_start_failover(&o_down_master, true));
        assert!(!should_start_failover(&o_down_master, false));

        o_down_master.flags.insert(InstanceFlags::FORCE_FAILOVER);
        assert!(should_start_failover(&o_down_master, false));
    }
}
