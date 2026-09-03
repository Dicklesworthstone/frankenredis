#![forbid(unsafe_code)]

use crate::{ASK_PERIOD_MS, SentinelRedisInstance, SentinelState};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ODownVote {
    pub sentinel_runid: String,
    pub is_down: bool,
    pub vote_time: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ODownResult {
    pub should_mark_o_down: bool,
    pub should_clear_o_down: bool,
    pub votes_for_down: u32,
    pub total_votes: u32,
}

pub fn evaluate_o_down(
    master: &SentinelRedisInstance,
    votes: &[ODownVote],
    now: u64,
) -> ODownResult {
    let quorum = master.quorum;

    let mut valid_votes: std::collections::HashMap<&str, &ODownVote> =
        std::collections::HashMap::new();
    for vote in votes
        .iter()
        .filter(|v| now.saturating_sub(v.vote_time) < ASK_PERIOD_MS * 5)
    {
        valid_votes
            .entry(vote.sentinel_runid.as_str())
            .and_modify(|existing| {
                if vote.vote_time >= existing.vote_time {
                    *existing = vote;
                }
            })
            .or_insert(vote);
    }

    let votes_for_down = valid_votes.values().filter(|v| v.is_down).count() as u32;
    let total_votes = valid_votes.len() as u32;

    let self_thinks_down = master.is_s_down();

    let effective_votes = if self_thinks_down {
        votes_for_down + 1
    } else {
        votes_for_down
    };

    let odown_now = quorum > 0 && master.is_s_down() && effective_votes >= quorum;
    let should_mark_o_down = !master.is_o_down() && odown_now;
    let should_clear_o_down = master.is_o_down() && !odown_now;

    ODownResult {
        should_mark_o_down,
        should_clear_o_down,
        votes_for_down,
        total_votes,
    }
}

pub fn apply_o_down_result(master: &mut SentinelRedisInstance, result: &ODownResult, now: u64) {
    if result.should_mark_o_down {
        master.set_o_down(true, now);
    } else if result.should_clear_o_down {
        master.set_o_down(false, now);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaderVote {
    pub voter_runid: String,
    pub leader_runid: String,
    pub epoch: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaderElectionResult {
    pub winner: Option<String>,
    pub votes_received: u32,
    pub votes_needed: u32,
    pub is_winner: bool,
}

pub fn evaluate_leader_election(
    my_runid: &str,
    current_epoch: u64,
    sentinel_count: u32,
    quorum: u32,
    votes: &[LeaderVote],
) -> LeaderElectionResult {
    let majority = (sentinel_count / 2) + 1;
    let votes_needed = majority.max(quorum);

    let mut votes_by_voter: std::collections::HashMap<&str, &str> =
        std::collections::HashMap::new();
    for vote in votes.iter().filter(|v| v.epoch == current_epoch) {
        votes_by_voter.insert(vote.voter_runid.as_str(), vote.leader_runid.as_str());
    }
    let mut vote_counts: std::collections::HashMap<&str, u32> = std::collections::HashMap::new();
    for leader_runid in votes_by_voter.values() {
        *vote_counts.entry(leader_runid).or_insert(0) += 1;
    }

    let my_votes = *vote_counts.get(my_runid).unwrap_or(&0);

    let winner = vote_counts
        .iter()
        .filter(|&(_, count)| *count >= votes_needed)
        .max_by_key(|&(_, count)| *count)
        .map(|(&runid, _)| runid.to_string());

    let is_winner = winner.as_deref() == Some(my_runid);

    LeaderElectionResult {
        winner,
        votes_received: my_votes,
        votes_needed,
        is_winner,
    }
}

/// One `SENTINEL IS-MASTER-DOWN-BY-ADDR` question fr-server must put to a peer
/// sentinel on this sentinel's behalf.
///
/// (frankenredis-rc-sentinel-peer-votes) Until this existed, `evaluate_o_down`
/// was always fed an empty vote slice and the leader election saw only the
/// self vote, so a master with a quorum above 1 could be `s_down` forever and
/// never fail over.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerAsk {
    pub master_name: String,
    pub sentinel_key: String,
    pub ip: String,
    pub port: u16,
    pub master_ip: String,
    pub master_port: u16,
    pub current_epoch: u64,
    /// `*` when only the peer's down state is wanted; this sentinel's run id
    /// when it is running a failover and needs the peer's vote.
    pub runid_arg: String,
}

/// A peer's reply: `[is_down, leader_runid | "*", leader_epoch]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerReply {
    pub is_down: bool,
    pub leader: Option<String>,
    pub leader_epoch: u64,
}

/// Mirrors `sentinelAskMasterStateToOtherSentinels`: peers are asked only about
/// a master this sentinel already sees S_DOWN, once per `ask_period` per peer,
/// and on every tick (forced) while a failover is in progress so votes arrive
/// promptly. The run id is sent only when a vote is wanted.
#[must_use]
pub fn peer_asks(state: &SentinelState, master_name: &str, now: u64) -> Vec<PeerAsk> {
    let Some(master) = state.get_master(master_name) else {
        return Vec::new();
    };
    if !master.is_s_down() {
        return Vec::new();
    }
    let in_failover = master
        .flags
        .contains(crate::InstanceFlags::FAILOVER_IN_PROGRESS);
    let ask_period = state.debug_config.ask_period;
    let myid = state.myid_hex();
    master
        .sentinels
        .iter()
        .filter(|(_, peer)| {
            in_failover || now.saturating_sub(peer.last_master_down_reply_time) >= ask_period
        })
        .map(|(key, peer)| PeerAsk {
            master_name: master_name.to_string(),
            sentinel_key: key.clone(),
            ip: peer.addr.ip.clone(),
            port: peer.addr.port,
            master_ip: master.addr.ip.clone(),
            master_port: master.addr.port,
            current_epoch: state.current_epoch,
            runid_arg: if in_failover {
                myid.clone()
            } else {
                "*".to_string()
            },
        })
        .collect()
}

/// Fold a peer's reply into its instance, as `sentinelReceiveIsMasterDownReply`
/// does: the reply time, the `MASTER_DOWN` flag, and the leader it voted for
/// when it answered a vote request. A failed ask (`None`) changes nothing and
/// is retried on the next period.
pub fn apply_peer_reply(
    state: &mut SentinelState,
    master_name: &str,
    sentinel_key: &str,
    reply: Option<PeerReply>,
    now: u64,
) {
    let Some(reply) = reply else {
        return;
    };
    let Some(master) = state.masters.get_mut(master_name) else {
        return;
    };
    let Some(peer) = master.sentinels.get_mut(sentinel_key) else {
        return;
    };
    peer.last_master_down_reply_time = now;
    if reply.is_down {
        peer.flags.insert(crate::InstanceFlags::MASTER_DOWN);
    } else {
        peer.flags.remove(crate::InstanceFlags::MASTER_DOWN);
    }
    if let Some(leader) = reply.leader.filter(|leader| leader != "*") {
        peer.leader = Some(leader);
        peer.leader_epoch = reply.leader_epoch;
    }
}

/// The peers' latest down/up answers as votes for `evaluate_o_down`, which
/// discards any older than five ask periods.
#[must_use]
pub fn peer_o_down_votes(master: &SentinelRedisInstance) -> Vec<ODownVote> {
    master
        .sentinels
        .values()
        .filter_map(|peer| {
            peer.runid.as_ref().map(|runid| ODownVote {
                sentinel_runid: runid.clone(),
                is_down: peer.flags.contains(crate::InstanceFlags::MASTER_DOWN),
                vote_time: peer.last_master_down_reply_time,
            })
        })
        .collect()
}

/// The peers' latest leader ballots for `evaluate_leader_election`, which
/// counts only the ones cast in the epoch being decided.
#[must_use]
pub fn peer_leader_votes(master: &SentinelRedisInstance) -> Vec<LeaderVote> {
    master
        .sentinels
        .values()
        .filter_map(|peer| match (&peer.runid, &peer.leader) {
            (Some(voter), Some(leader)) => Some(LeaderVote {
                voter_runid: voter.clone(),
                leader_runid: leader.clone(),
                epoch: peer.leader_epoch,
            }),
            _ => None,
        })
        .collect()
}

pub fn should_request_vote(master: &SentinelRedisInstance, my_epoch: u64, _now: u64) -> bool {
    if !master.is_o_down() {
        return false;
    }
    if master.failover_state != crate::FailoverState::None
        && master.failover_state != crate::FailoverState::WaitStart
    {
        return false;
    }
    if master.leader.is_some() && master.leader_epoch >= my_epoch {
        return false;
    }
    true
}

pub fn cast_vote(
    state: &mut SentinelState,
    master_name: &str,
    candidate_runid: &str,
    candidate_epoch: u64,
) -> Option<String> {
    if candidate_epoch < state.current_epoch {
        return None;
    }

    let current_leader_epoch = state
        .get_master(master_name)
        .map(|m| m.leader_epoch)
        .unwrap_or(0);
    let current_leader = state.get_master(master_name).and_then(|m| m.leader.clone());

    if current_leader_epoch >= candidate_epoch {
        return current_leader;
    }

    if candidate_epoch > state.current_epoch {
        state.current_epoch = candidate_epoch;
    }

    let master = state.get_master_mut(master_name)?;
    master.leader = Some(candidate_runid.to_string());
    master.leader_epoch = candidate_epoch;

    Some(candidate_runid.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{InstanceFlags, SentinelAddr};

    fn make_master() -> SentinelRedisInstance {
        let addr = SentinelAddr::new("127.0.0.1", 6379);
        SentinelRedisInstance::new_master("mymaster", addr, 2)
    }

    #[test]
    fn o_down_requires_quorum() {
        let mut master = make_master();
        master.flags.insert(InstanceFlags::S_DOWN);

        let votes = vec![ODownVote {
            sentinel_runid: "s1".to_string(),
            is_down: true,
            vote_time: 1000,
        }];

        let result = evaluate_o_down(&master, &votes, 2000);
        assert!(result.should_mark_o_down);
        assert_eq!(result.votes_for_down, 1);
    }

    #[test]
    fn o_down_zero_quorum_fails_closed() {
        let mut master =
            SentinelRedisInstance::new_master("mymaster", SentinelAddr::new("127.0.0.1", 6379), 0);
        master.flags.insert(InstanceFlags::S_DOWN);

        let result = evaluate_o_down(&master, &[], 2000);
        assert!(!result.should_mark_o_down);
        assert_eq!(result.votes_for_down, 0);
        assert_eq!(result.total_votes, 0);
    }

    #[test]
    fn o_down_clears_when_votes_fall_below_quorum_even_if_s_down_remains() {
        let mut master =
            SentinelRedisInstance::new_master("mymaster", SentinelAddr::new("127.0.0.1", 6379), 3);
        master.flags.insert(InstanceFlags::S_DOWN);
        master.flags.insert(InstanceFlags::O_DOWN);

        let votes = vec![ODownVote {
            sentinel_runid: "s1".to_string(),
            is_down: true,
            vote_time: 1000,
        }];
        let result = evaluate_o_down(&master, &votes, 2000);

        assert!(!result.should_mark_o_down);
        assert!(result.should_clear_o_down);
        assert_eq!(result.votes_for_down, 1);
        assert_eq!(result.total_votes, 1);
    }

    #[test]
    fn o_down_counts_each_sentinel_vote_once() {
        let mut master =
            SentinelRedisInstance::new_master("mymaster", SentinelAddr::new("127.0.0.1", 6379), 3);
        master.flags.insert(InstanceFlags::S_DOWN);

        let votes = vec![
            ODownVote {
                sentinel_runid: "s1".to_string(),
                is_down: true,
                vote_time: 1000,
            },
            ODownVote {
                sentinel_runid: "s1".to_string(),
                is_down: true,
                vote_time: 2000,
            },
        ];
        let result = evaluate_o_down(&master, &votes, 3000);

        assert!(!result.should_mark_o_down);
        assert_eq!(result.votes_for_down, 1);
        assert_eq!(result.total_votes, 1);
    }

    #[test]
    fn o_down_not_marked_without_s_down() {
        let master = make_master();

        let votes = vec![
            ODownVote {
                sentinel_runid: "s1".to_string(),
                is_down: true,
                vote_time: 1000,
            },
            ODownVote {
                sentinel_runid: "s2".to_string(),
                is_down: true,
                vote_time: 1000,
            },
        ];

        let result = evaluate_o_down(&master, &votes, 2000);
        assert!(!result.should_mark_o_down);
    }

    #[test]
    fn o_down_cleared_when_s_down_cleared() {
        let mut master = make_master();
        master.flags.insert(InstanceFlags::O_DOWN);

        let votes = vec![];
        let result = evaluate_o_down(&master, &votes, 2000);
        assert!(result.should_clear_o_down);
    }

    #[test]
    fn leader_election_majority_wins() {
        let votes = vec![
            LeaderVote {
                voter_runid: "s1".to_string(),
                leader_runid: "s2".to_string(),
                epoch: 1,
            },
            LeaderVote {
                voter_runid: "s2".to_string(),
                leader_runid: "s2".to_string(),
                epoch: 1,
            },
            LeaderVote {
                voter_runid: "s3".to_string(),
                leader_runid: "s2".to_string(),
                epoch: 1,
            },
        ];

        let result = evaluate_leader_election("s2", 1, 5, 2, &votes);
        assert_eq!(result.winner, Some("s2".to_string()));
        assert!(result.is_winner);
        assert_eq!(result.votes_received, 3);
        assert_eq!(result.votes_needed, 3);
    }

    #[test]
    fn leader_election_no_majority() {
        let votes = vec![
            LeaderVote {
                voter_runid: "s1".to_string(),
                leader_runid: "s1".to_string(),
                epoch: 1,
            },
            LeaderVote {
                voter_runid: "s2".to_string(),
                leader_runid: "s2".to_string(),
                epoch: 1,
            },
        ];

        let result = evaluate_leader_election("s1", 1, 5, 2, &votes);
        assert!(result.winner.is_none());
        assert!(!result.is_winner);
    }

    #[test]
    fn leader_election_requires_quorum_even_after_majority() {
        let votes = vec![
            LeaderVote {
                voter_runid: "s1".to_string(),
                leader_runid: "candidate".to_string(),
                epoch: 1,
            },
            LeaderVote {
                voter_runid: "s2".to_string(),
                leader_runid: "candidate".to_string(),
                epoch: 1,
            },
            LeaderVote {
                voter_runid: "s3".to_string(),
                leader_runid: "candidate".to_string(),
                epoch: 1,
            },
        ];

        let result = evaluate_leader_election("candidate", 1, 5, 4, &votes);
        assert!(result.winner.is_none());
        assert!(!result.is_winner);
        assert_eq!(result.votes_received, 3);
        assert_eq!(result.votes_needed, 4);
    }

    #[test]
    fn leader_election_counts_each_voter_once() {
        let votes = vec![
            LeaderVote {
                voter_runid: "s1".to_string(),
                leader_runid: "candidate".to_string(),
                epoch: 1,
            },
            LeaderVote {
                voter_runid: "s1".to_string(),
                leader_runid: "candidate".to_string(),
                epoch: 1,
            },
            LeaderVote {
                voter_runid: "s1".to_string(),
                leader_runid: "candidate".to_string(),
                epoch: 1,
            },
        ];

        let result = evaluate_leader_election("candidate", 1, 5, 2, &votes);
        assert!(result.winner.is_none());
        assert_eq!(result.votes_received, 1);
    }

    #[test]
    fn cast_vote_updates_leader() {
        let mut state = SentinelState::new();
        state.monitor("mymaster", "127.0.0.1", 6379, 2).unwrap();

        let voted_for = cast_vote(&mut state, "mymaster", "candidate1", 1);
        assert_eq!(voted_for, Some("candidate1".to_string()));

        let master = state.get_master("mymaster").unwrap();
        assert_eq!(master.leader, Some("candidate1".to_string()));
        assert_eq!(master.leader_epoch, 1);
    }

    #[test]
    fn cast_vote_rejects_old_epoch() {
        let mut state = SentinelState::new();
        state.current_epoch = 5;
        state.monitor("mymaster", "127.0.0.1", 6379, 2).unwrap();

        let voted_for = cast_vote(&mut state, "mymaster", "candidate1", 3);
        assert!(voted_for.is_none());
    }

    #[test]
    fn should_request_vote_checks_o_down() {
        let master = make_master();
        assert!(!should_request_vote(&master, 1, 1000));

        let mut o_down_master = make_master();
        o_down_master.flags.insert(InstanceFlags::O_DOWN);
        assert!(should_request_vote(&o_down_master, 1, 1000));
    }
}
