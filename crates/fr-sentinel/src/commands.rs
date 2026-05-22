#![forbid(unsafe_code)]

use std::{fs, os::unix::fs::PermissionsExt};

use crate::SentinelState;
use fr_protocol::RespFrame;

pub fn dispatch_sentinel_command(state: &mut SentinelState, args: &[&[u8]]) -> RespFrame {
    if args.is_empty() {
        return RespFrame::Error("ERR wrong number of arguments for 'sentinel' command".into());
    }

    let subcommand = String::from_utf8_lossy(args[0]).to_ascii_uppercase();
    match subcommand.as_str() {
        "MYID" => {
            if args.len() != 1 {
                return wrong_arity("sentinel myid");
            }
            cmd_myid(state)
        }
        "MASTERS" => {
            if args.len() != 1 {
                return wrong_arity("sentinel masters");
            }
            cmd_masters(state)
        }
        "MASTER" => cmd_master(state, &args[1..]),
        "REPLICAS" | "SLAVES" => cmd_replicas(state, &args[1..]),
        "SENTINELS" => cmd_sentinels(state, &args[1..]),
        "IS-MASTER-DOWN-BY-ADDR" => cmd_is_master_down_by_addr(state, &args[1..]),
        "MONITOR" => cmd_monitor(state, &args[1..]),
        "REMOVE" => cmd_remove(state, &args[1..]),
        "SET" => cmd_set(state, &args[1..]),
        "RESET" => cmd_reset(state, &args[1..]),
        "GET-MASTER-ADDR-BY-NAME" => cmd_get_master_addr(state, &args[1..]),
        "CKQUORUM" => cmd_ckquorum(state, &args[1..]),
        "CONFIG" => cmd_config(state, &args[1..]),
        "FLUSHCONFIG" => {
            if args.len() != 1 {
                return wrong_arity("sentinel flushconfig");
            }
            cmd_flushconfig(state)
        }
        "FAILOVER" => cmd_failover(state, &args[1..]),
        "PENDING-SCRIPTS" => {
            if args.len() != 1 {
                return wrong_arity("sentinel pending-scripts");
            }
            cmd_pending_scripts(state)
        }
        "INFO-CACHE" => cmd_info_cache(state, &args[1..]),
        "SIMULATE-FAILURE" => cmd_simulate_failure(state, &args[1..]),
        "DEBUG" => cmd_debug(state, &args[1..]),
        "HELP" => {
            if args.len() != 1 {
                return wrong_arity("sentinel help");
            }
            cmd_help()
        }
        _ => RespFrame::Error(format!("ERR Unknown sentinel subcommand '{}'", subcommand)),
    }
}

fn wrong_arity(command: &'static str) -> RespFrame {
    RespFrame::Error(format!(
        "ERR wrong number of arguments for '{command}' command"
    ))
}

fn cmd_myid(state: &SentinelState) -> RespFrame {
    RespFrame::BulkString(Some(state.myid_hex().into_bytes()))
}

fn cmd_masters(state: &SentinelState) -> RespFrame {
    let masters = sorted_instance_info_arrays(state.masters.values());
    RespFrame::Array(Some(masters))
}

fn cmd_master(state: &SentinelState, args: &[&[u8]]) -> RespFrame {
    if args.len() != 1 {
        return wrong_arity("sentinel master");
    }
    let name = String::from_utf8_lossy(args[0]);
    match state.get_master(&name) {
        Some(master) => instance_to_info_array(master),
        None => RespFrame::Error(format!("ERR No such master with that name: {}", name)),
    }
}

fn cmd_replicas(state: &SentinelState, args: &[&[u8]]) -> RespFrame {
    if args.len() != 1 {
        return wrong_arity("sentinel replicas");
    }
    let name = String::from_utf8_lossy(args[0]);
    match state.get_master(&name) {
        Some(master) => {
            let replicas = sorted_instance_info_arrays(master.slaves.values());
            RespFrame::Array(Some(replicas))
        }
        None => RespFrame::Error(format!("ERR No such master with that name: {}", name)),
    }
}

fn cmd_sentinels(state: &SentinelState, args: &[&[u8]]) -> RespFrame {
    if args.len() != 1 {
        return wrong_arity("sentinel sentinels");
    }
    let name = String::from_utf8_lossy(args[0]);
    match state.get_master(&name) {
        Some(master) => {
            let sentinels = sorted_instance_info_arrays(master.sentinels.values());
            RespFrame::Array(Some(sentinels))
        }
        None => RespFrame::Error(format!("ERR No such master with that name: {}", name)),
    }
}

fn cmd_is_master_down_by_addr(state: &mut SentinelState, args: &[&[u8]]) -> RespFrame {
    if args.len() != 4 {
        return wrong_arity("sentinel is-master-down-by-addr");
    }

    let ip = String::from_utf8_lossy(args[0]);
    let port = match String::from_utf8_lossy(args[1]).parse::<u16>() {
        Ok(port) => port,
        Err(_) => return RespFrame::Error("ERR Invalid port number".into()),
    };
    let requested_epoch = match String::from_utf8_lossy(args[2]).parse::<u64>() {
        Ok(epoch) => epoch,
        Err(_) => return RespFrame::Error("ERR Invalid current epoch".into()),
    };
    let requested_runid = String::from_utf8_lossy(args[3]);

    let master_name = find_master_name_by_addr(state, &ip, port);
    let is_down = master_name
        .as_deref()
        .and_then(|name| state.get_master(name))
        .is_some_and(|master| {
            !state.tilt && master.is_master() && master.flags.contains(crate::InstanceFlags::S_DOWN)
        });

    let (leader, leader_epoch) = if requested_runid == "*" {
        (None, 0)
    } else if let Some(master_name) = master_name {
        sentinel_vote_leader(state, &master_name, requested_epoch, &requested_runid)
    } else {
        (None, 0)
    };

    RespFrame::Array(Some(vec![
        RespFrame::Integer(i64::from(is_down)),
        RespFrame::BulkString(Some(leader.unwrap_or_else(|| "*".to_string()).into_bytes())),
        RespFrame::Integer(i64::try_from(leader_epoch).unwrap_or(i64::MAX)),
    ]))
}

fn find_master_name_by_addr(state: &SentinelState, ip: &str, port: u16) -> Option<String> {
    state
        .masters
        .values()
        .find(|master| master.addr.port == port && master.addr.hostname.eq_ignore_ascii_case(ip))
        .map(|master| master.name.clone())
}

fn sentinel_vote_leader(
    state: &mut SentinelState,
    master_name: &str,
    requested_epoch: u64,
    requested_runid: &str,
) -> (Option<String>, u64) {
    if requested_epoch > state.current_epoch {
        state.current_epoch = requested_epoch;
    }
    let current_epoch = state.current_epoch;
    let Some(master) = state.get_master_mut(master_name) else {
        return (None, 0);
    };

    if master.leader_epoch < requested_epoch && current_epoch <= requested_epoch {
        master.leader = Some(requested_runid.to_string());
        master.leader_epoch = current_epoch;
    }

    (master.leader.clone(), master.leader_epoch)
}

fn cmd_monitor(state: &mut SentinelState, args: &[&[u8]]) -> RespFrame {
    if args.len() != 4 {
        return wrong_arity("sentinel monitor");
    }
    let name = String::from_utf8_lossy(args[0]);
    let ip = String::from_utf8_lossy(args[1]);
    let port: u16 = match String::from_utf8_lossy(args[2]).parse() {
        Ok(p) => p,
        Err(_) => return RespFrame::Error("ERR Invalid port number".into()),
    };
    let quorum_raw = String::from_utf8_lossy(args[3]);
    let quorum = match parse_monitor_quorum(&quorum_raw) {
        Ok(q) => q,
        Err(error) => return error,
    };
    if !monitor_address_is_allowed(state, &ip) {
        return RespFrame::Error("ERR Invalid IP address or hostname specified".into());
    }

    match state.monitor(name.as_ref(), ip.as_ref(), port, quorum) {
        Ok(()) => RespFrame::SimpleString("OK".into()),
        Err(e) => RespFrame::Error(e.into()),
    }
}

fn monitor_address_is_allowed(state: &SentinelState, value: &str) -> bool {
    state.resolve_hostnames || value.parse::<std::net::IpAddr>().is_ok()
}

fn parse_monitor_quorum(value: &str) -> Result<u32, RespFrame> {
    let parsed = value
        .parse::<i64>()
        .map_err(|_| RespFrame::Error("ERR Invalid quorum number".into()))?;
    if parsed <= 0 {
        return Err(RespFrame::Error("ERR Quorum must be 1 or greater.".into()));
    }
    u32::try_from(parsed).map_err(|_| RespFrame::Error("ERR Invalid quorum number".into()))
}

fn cmd_remove(state: &mut SentinelState, args: &[&[u8]]) -> RespFrame {
    if args.len() != 1 {
        return wrong_arity("sentinel remove");
    }
    let name = String::from_utf8_lossy(args[0]);
    match state.remove(&name) {
        Ok(()) => RespFrame::SimpleString("OK".into()),
        Err(e) => RespFrame::Error(e.into()),
    }
}

fn cmd_set(state: &mut SentinelState, args: &[&[u8]]) -> RespFrame {
    if args.len() < 3 {
        return RespFrame::Error("ERR wrong number of arguments for 'sentinel set' command".into());
    }
    let name = String::from_utf8_lossy(args[0]);
    let deny_scripts_reconfig = state.deny_scripts_reconfig;
    let master = match state.get_master_mut(&name) {
        Some(m) => m,
        None => return RespFrame::Error(format!("ERR No such master with that name: {}", name)),
    };

    let mut i = 1;
    while i < args.len() {
        let option_raw = String::from_utf8_lossy(args[i]);
        let option = option_raw.to_ascii_lowercase();

        match option.as_str() {
            "down-after-milliseconds" => {
                let value = match sentinel_set_value(args, i, &option_raw) {
                    Ok(value) => value,
                    Err(error) => return error,
                };
                master.down_after_period = match parse_positive_u64(&value, &option_raw) {
                    Ok(parsed) => parsed,
                    Err(error) => return error,
                };
                i += 2;
            }
            "failover-timeout" => {
                let value = match sentinel_set_value(args, i, &option_raw) {
                    Ok(value) => value,
                    Err(error) => return error,
                };
                master.failover_timeout = match parse_positive_u64(&value, &option_raw) {
                    Ok(parsed) => parsed,
                    Err(error) => return error,
                };
                i += 2;
            }
            "parallel-syncs" => {
                let value = match sentinel_set_value(args, i, &option_raw) {
                    Ok(value) => value,
                    Err(error) => return error,
                };
                master.parallel_syncs = match parse_positive_u32(&value, &option_raw) {
                    Ok(parsed) => parsed,
                    Err(error) => return error,
                };
                i += 2;
            }
            "quorum" => {
                let value = match sentinel_set_value(args, i, &option_raw) {
                    Ok(value) => value,
                    Err(error) => return error,
                };
                master.quorum = match parse_positive_u32(&value, &option_raw) {
                    Ok(parsed) => parsed,
                    Err(error) => return error,
                };
                i += 2;
            }
            "master-reboot-down-after-period" => {
                let value = match sentinel_set_value(args, i, &option_raw) {
                    Ok(value) => value,
                    Err(error) => return error,
                };
                master.master_reboot_down_after_period =
                    match parse_non_negative_u64(&value, &option_raw) {
                        Ok(parsed) => parsed,
                        Err(error) => return error,
                    };
                i += 2;
            }
            "auth-pass" => {
                let value = match sentinel_set_value(args, i, &option_raw) {
                    Ok(value) => value,
                    Err(error) => return error,
                };
                master.auth_pass = if value.is_empty() {
                    None
                } else {
                    Some(value.into_owned())
                };
                i += 2;
            }
            "auth-user" => {
                let value = match sentinel_set_value(args, i, &option_raw) {
                    Ok(value) => value,
                    Err(error) => return error,
                };
                master.auth_user = if value.is_empty() {
                    None
                } else {
                    Some(value.into_owned())
                };
                i += 2;
            }
            "rename-command" => {
                let Some(oldname) = args.get(i + 1) else {
                    return unknown_sentinel_set_option(&option_raw);
                };
                let Some(newname) = args.get(i + 2) else {
                    return unknown_sentinel_set_option(&option_raw);
                };
                let oldname = String::from_utf8_lossy(oldname);
                let newname = String::from_utf8_lossy(newname);
                if oldname.is_empty() {
                    return invalid_sentinel_set_argument(&oldname, &option_raw);
                }
                if newname.is_empty() {
                    return invalid_sentinel_set_argument(&newname, &option_raw);
                }
                set_renamed_command(master, &oldname, &newname);
                i += 3;
            }
            "notification-script" => {
                let value = match sentinel_set_value(args, i, &option_raw) {
                    Ok(value) => value,
                    Err(error) => return error,
                };
                if deny_scripts_reconfig {
                    return RespFrame::Error(
                        "ERR Reconfiguration of scripts path is denied for security reasons. Check the deny-scripts-reconfig configuration directive in your Sentinel configuration".into(),
                    );
                }
                if !value.is_empty() && !path_has_execute_permission(&value) {
                    return RespFrame::Error(
                        "ERR Notification script seems non existing or non executable".into(),
                    );
                }
                master.notification_script = if value.is_empty() {
                    None
                } else {
                    Some(value.into_owned())
                };
                i += 2;
            }
            "client-reconfig-script" => {
                let value = match sentinel_set_value(args, i, &option_raw) {
                    Ok(value) => value,
                    Err(error) => return error,
                };
                if deny_scripts_reconfig {
                    return RespFrame::Error(
                        "ERR Reconfiguration of scripts path is denied for security reasons. Check the deny-scripts-reconfig configuration directive in your Sentinel configuration".into(),
                    );
                }
                if !value.is_empty() && !path_has_execute_permission(&value) {
                    return RespFrame::Error(
                        "ERR Client reconfiguration script seems non existing or non executable"
                            .into(),
                    );
                }
                master.client_reconfig_script = if value.is_empty() {
                    None
                } else {
                    Some(value.into_owned())
                };
                i += 2;
            }
            _ => {
                return unknown_sentinel_set_option(&option_raw);
            }
        }
    }
    RespFrame::SimpleString("OK".into())
}

fn sentinel_set_value<'a>(
    args: &'a [&[u8]],
    option_index: usize,
    option: &str,
) -> Result<std::borrow::Cow<'a, str>, RespFrame> {
    args.get(option_index + 1)
        .map(|value| String::from_utf8_lossy(value))
        .ok_or_else(|| unknown_sentinel_set_option(option))
}

fn set_renamed_command(master: &mut crate::SentinelRedisInstance, oldname: &str, newname: &str) {
    let old_key = oldname.to_ascii_lowercase();
    master.renamed_commands.remove(&old_key);
    if !oldname.eq_ignore_ascii_case(newname) {
        master.renamed_commands.insert(old_key, newname.to_string());
    }
}

fn unknown_sentinel_set_option(option: &str) -> RespFrame {
    RespFrame::Error(format!(
        "ERR Unknown option or number of arguments for SENTINEL SET '{option}'"
    ))
}

fn path_has_execute_permission(path: &str) -> bool {
    fs::metadata(path).is_ok_and(|metadata| metadata.permissions().mode() & 0o111 != 0)
}

fn parse_positive_u64(value: &str, option: &str) -> Result<u64, RespFrame> {
    value
        .parse::<u64>()
        .ok()
        .filter(|parsed| *parsed > 0)
        .ok_or_else(|| invalid_sentinel_set_argument(value, option))
}

fn parse_positive_u32(value: &str, option: &str) -> Result<u32, RespFrame> {
    value
        .parse::<u32>()
        .ok()
        .filter(|parsed| *parsed > 0)
        .ok_or_else(|| invalid_sentinel_set_argument(value, option))
}

fn parse_non_negative_u64(value: &str, option: &str) -> Result<u64, RespFrame> {
    value
        .parse::<u64>()
        .map_err(|_| invalid_sentinel_set_argument(value, option))
}

fn invalid_sentinel_set_argument(value: &str, option: &str) -> RespFrame {
    RespFrame::Error(format!(
        "ERR Invalid argument '{value}' for SENTINEL SET '{option}'"
    ))
}

fn cmd_reset(state: &mut SentinelState, args: &[&[u8]]) -> RespFrame {
    if args.len() != 1 {
        return wrong_arity("sentinel reset");
    }
    let pattern = String::from_utf8_lossy(args[0]);
    let mut count = 0i64;

    let names_to_reset: Vec<String> = state
        .masters
        .keys()
        .filter(|name| glob_match(&pattern, name))
        .cloned()
        .collect();

    for name in names_to_reset {
        if let Some(master) = state.masters.get_mut(&name) {
            master.sentinels.clear();
            master.slaves.clear();
            count += 1;
        }
    }
    RespFrame::Integer(count)
}

fn cmd_get_master_addr(state: &SentinelState, args: &[&[u8]]) -> RespFrame {
    if args.len() != 1 {
        return wrong_arity("sentinel get-master-addr-by-name");
    }
    let name = String::from_utf8_lossy(args[0]);
    match state.get_master(&name) {
        Some(master) => RespFrame::Array(Some(vec![
            RespFrame::BulkString(Some(master.addr.hostname.clone().into_bytes())),
            RespFrame::BulkString(Some(master.addr.port.to_string().into_bytes())),
        ])),
        None => RespFrame::BulkString(None),
    }
}

fn cmd_ckquorum(state: &SentinelState, args: &[&[u8]]) -> RespFrame {
    if args.len() != 1 {
        return wrong_arity("sentinel ckquorum");
    }
    let name = String::from_utf8_lossy(args[0]);
    match state.get_master(&name) {
        Some(master) => {
            let sentinel_count = master.sentinels.len() as u32 + 1;
            if sentinel_count >= master.quorum {
                RespFrame::SimpleString(format!(
                    "OK {} usable Sentinels. Quorum and failover authorization is possible",
                    sentinel_count
                ))
            } else {
                RespFrame::Error(format!(
                    "NOQUORUM {} Sentinels known, {} needed for quorum",
                    sentinel_count, master.quorum
                ))
            }
        }
        None => RespFrame::Error(format!("ERR No such master with that name: {}", name)),
    }
}

const SENTINEL_CONFIG_KEYS: [&str; 7] = [
    "resolve-hostnames",
    "announce-hostnames",
    "announce-ip",
    "announce-port",
    "sentinel-user",
    "sentinel-pass",
    "loglevel",
];

fn cmd_config(state: &mut SentinelState, args: &[&[u8]]) -> RespFrame {
    if args.len() < 2 {
        return wrong_arity("sentinel config");
    }

    let subcommand = String::from_utf8_lossy(args[0]).to_ascii_uppercase();
    match subcommand.as_str() {
        "GET" if args.len() >= 2 => cmd_config_get(state, &args[1..]),
        "SET" if args.len() >= 3 => cmd_config_set(state, &args[1..]),
        _ => RespFrame::Error(
            "ERR Only SENTINEL CONFIG GET <param> [<param> <param> ...]/ SET <param> <value> [<param> <value> ...] are supported.".into(),
        ),
    }
}

fn cmd_config_get(state: &SentinelState, patterns: &[&[u8]]) -> RespFrame {
    let mut emitted = Vec::new();
    let mut reply = Vec::new();

    for raw_pattern in patterns {
        let pattern = String::from_utf8_lossy(raw_pattern);
        for key in SENTINEL_CONFIG_KEYS {
            if emitted.contains(&key) || !glob_match_ignore_ascii_case(&pattern, key) {
                continue;
            }
            reply.push((
                RespFrame::BulkString(Some(key.as_bytes().to_vec())),
                RespFrame::BulkString(Some(sentinel_config_value(state, key).into_bytes())),
            ));
            emitted.push(key);
        }
    }

    RespFrame::Map(Some(reply))
}

fn sentinel_config_value(state: &SentinelState, key: &str) -> String {
    match key {
        "resolve-hostnames" => yes_no(state.resolve_hostnames).to_string(),
        "announce-hostnames" => yes_no(state.announce_hostnames).to_string(),
        "announce-ip" => state.announce_ip.clone().unwrap_or_default(),
        "announce-port" => state.announce_port.unwrap_or(0).to_string(),
        "sentinel-user" => state.sentinel_auth_user.clone().unwrap_or_default(),
        "sentinel-pass" => state.sentinel_auth_pass.clone().unwrap_or_default(),
        "loglevel" => state.loglevel.clone(),
        _ => String::new(),
    }
}

fn cmd_config_set(state: &mut SentinelState, args: &[&[u8]]) -> RespFrame {
    let mut seen = Vec::new();
    let mut updates = Vec::new();
    let mut cursor = 0usize;

    while cursor < args.len() {
        let option_raw = String::from_utf8_lossy(args[cursor]);
        let Some(option) = canonical_sentinel_config_key(&option_raw) else {
            return RespFrame::Error(format!(
                "ERR Invalid argument '{}' to SENTINEL CONFIG SET",
                option_raw
            ));
        };
        if seen.contains(&option) {
            return RespFrame::Error(format!(
                "ERR Duplicate argument '{}' to SENTINEL CONFIG SET",
                option_raw
            ));
        }
        if cursor + 1 == args.len() {
            return RespFrame::Error(format!("ERR Missing argument '{}' value", option_raw));
        }

        let value = String::from_utf8_lossy(args[cursor + 1]).into_owned();
        if !sentinel_config_value_is_valid(option, &value) {
            return RespFrame::Error(format!(
                "ERR Invalid value '{value}' to SENTINEL CONFIG SET '{option_raw}'"
            ));
        }

        seen.push(option);
        updates.push((option, value));
        cursor += 2;
    }

    for (option, value) in updates {
        apply_sentinel_config_update(state, option, value);
    }
    RespFrame::SimpleString("OK".into())
}

fn canonical_sentinel_config_key(option: &str) -> Option<&'static str> {
    SENTINEL_CONFIG_KEYS
        .into_iter()
        .find(|key| key.eq_ignore_ascii_case(option))
}

fn sentinel_config_value_is_valid(option: &str, value: &str) -> bool {
    match option {
        "resolve-hostnames" | "announce-hostnames" => parse_yes_no(value).is_some(),
        "announce-port" => value
            .parse::<i64>()
            .is_ok_and(|parsed| (0..=65_535).contains(&parsed)),
        "loglevel" => matches!(
            value.to_ascii_lowercase().as_str(),
            "debug" | "verbose" | "notice" | "warning" | "nothing"
        ),
        "announce-ip" | "sentinel-user" | "sentinel-pass" => true,
        _ => false,
    }
}

fn apply_sentinel_config_update(state: &mut SentinelState, option: &str, value: String) {
    match option {
        "resolve-hostnames" => {
            state.resolve_hostnames = parse_yes_no(&value).unwrap_or(false);
        }
        "announce-hostnames" => {
            state.announce_hostnames = parse_yes_no(&value).unwrap_or(false);
        }
        "announce-ip" => {
            state.announce_ip = Some(value);
        }
        "announce-port" => {
            state.announce_port = value.parse::<u16>().ok();
        }
        "sentinel-user" => {
            state.sentinel_auth_user = if value.is_empty() { None } else { Some(value) };
        }
        "sentinel-pass" => {
            state.sentinel_auth_pass = if value.is_empty() { None } else { Some(value) };
        }
        "loglevel" => {
            state.loglevel = value.to_ascii_lowercase();
        }
        _ => {}
    }
}

fn parse_yes_no(value: &str) -> Option<bool> {
    if value.eq_ignore_ascii_case("yes") {
        Some(true)
    } else if value.eq_ignore_ascii_case("no") {
        Some(false)
    } else {
        None
    }
}

fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

fn cmd_flushconfig(_state: &SentinelState) -> RespFrame {
    RespFrame::SimpleString("OK".into())
}

fn cmd_failover(state: &mut SentinelState, args: &[&[u8]]) -> RespFrame {
    if args.len() != 1 {
        return wrong_arity("sentinel failover");
    }
    let name = String::from_utf8_lossy(args[0]);
    let now = state.previous_time;
    let new_epoch = state.current_epoch.saturating_add(1);
    let mut started = false;
    let reply = match state.get_master_mut(&name) {
        Some(master) => {
            use crate::{FailoverState, InstanceFlags};
            if master.flags.contains(InstanceFlags::FAILOVER_IN_PROGRESS) {
                return RespFrame::Error("-INPROG Failover already in progress".into());
            }
            if !has_suitable_failover_replica(master) {
                return RespFrame::Error("-NOGOODSLAVE No suitable replica to promote".into());
            }
            master.flags.insert(InstanceFlags::FORCE_FAILOVER);
            master.flags.insert(InstanceFlags::FAILOVER_IN_PROGRESS);
            master.failover_epoch = new_epoch;
            master.failover_start_time = now;
            master.failover_state_change_time = now;
            master.failover_state = FailoverState::WaitStart;
            started = true;
            RespFrame::SimpleString("OK".into())
        }
        None => RespFrame::Error(format!("ERR No such master with that name: {}", name)),
    };
    if started {
        state.current_epoch = new_epoch;
    }
    reply
}

fn has_suitable_failover_replica(master: &crate::SentinelRedisInstance) -> bool {
    master.slaves.values().any(is_suitable_failover_replica)
}

fn is_suitable_failover_replica(replica: &crate::SentinelRedisInstance) -> bool {
    use crate::InstanceFlags;

    !replica.flags.contains(InstanceFlags::S_DOWN)
        && !replica.flags.contains(InstanceFlags::O_DOWN)
        && !replica.link.disconnected
        && replica.slave_priority > 0
}

fn cmd_pending_scripts(state: &SentinelState) -> RespFrame {
    let scripts: Vec<RespFrame> = state
        .scripts_queue
        .iter()
        .map(pending_script_job_reply)
        .collect();
    RespFrame::Array(Some(scripts))
}

fn pending_script_job_reply(script: &crate::ScriptJob) -> RespFrame {
    let mut argv = Vec::with_capacity(script.args.len() + 1);
    argv.push(RespFrame::BulkString(Some(
        script.path.clone().into_bytes(),
    )));
    argv.extend(
        script
            .args
            .iter()
            .map(|arg| RespFrame::BulkString(Some(arg.clone().into_bytes()))),
    );

    RespFrame::Map(Some(vec![
        (
            RespFrame::BulkString(Some(b"argv".to_vec())),
            RespFrame::Array(Some(argv)),
        ),
        (
            RespFrame::BulkString(Some(b"flags".to_vec())),
            RespFrame::BulkString(Some(b"scheduled".to_vec())),
        ),
        (
            RespFrame::BulkString(Some(b"pid".to_vec())),
            RespFrame::BulkString(Some(b"0".to_vec())),
        ),
        (
            RespFrame::BulkString(Some(b"run-delay".to_vec())),
            RespFrame::BulkString(Some(b"0".to_vec())),
        ),
        (
            RespFrame::BulkString(Some(b"retry-num".to_vec())),
            RespFrame::BulkString(Some(script.retry_count.to_string().into_bytes())),
        ),
    ]))
}

fn cmd_info_cache(state: &SentinelState, args: &[&[u8]]) -> RespFrame {
    let mut masters: Vec<&crate::SentinelRedisInstance> = if args.is_empty() {
        state.masters.values().collect()
    } else {
        args.iter()
            .filter_map(|name| {
                let name = String::from_utf8_lossy(name);
                state.get_master(&name)
            })
            .collect()
    };
    masters.sort_by(|left, right| left.name.cmp(&right.name));
    masters.dedup_by(|left, right| left.name == right.name);

    let mut reply = Vec::with_capacity(masters.len() * 2);
    for master in masters {
        reply.push(RespFrame::BulkString(Some(master.name.as_bytes().to_vec())));
        reply.push(RespFrame::Array(Some(info_cache_rows(
            master,
            state.previous_time,
        ))));
    }
    RespFrame::Array(Some(reply))
}

fn cmd_simulate_failure(state: &mut SentinelState, args: &[&[u8]]) -> RespFrame {
    state.simfailure_flags = crate::SimFailureFlags::empty();

    for arg in args {
        let option = String::from_utf8_lossy(arg);
        if option.eq_ignore_ascii_case("crash-after-election") {
            state
                .simfailure_flags
                .insert(crate::SimFailureFlags::CRASH_AFTER_ELECTION);
        } else if option.eq_ignore_ascii_case("crash-after-promotion") {
            state
                .simfailure_flags
                .insert(crate::SimFailureFlags::CRASH_AFTER_PROMOTION);
        } else if option.eq_ignore_ascii_case("help") {
            return RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"crash-after-election".to_vec())),
                RespFrame::BulkString(Some(b"crash-after-promotion".to_vec())),
            ]));
        } else {
            return RespFrame::Error("ERR Unknown failure simulation specified".into());
        }
    }

    RespFrame::SimpleString("OK".into())
}

fn info_cache_rows(instance: &crate::SentinelRedisInstance, now_ms: u64) -> Vec<RespFrame> {
    let mut rows = Vec::with_capacity(instance.slaves.len() + 1);
    rows.push(info_cache_row(instance, now_ms));

    let mut replicas: Vec<_> = instance.slaves.values().collect();
    replicas.sort_by(|left, right| left.name.cmp(&right.name));
    rows.extend(
        replicas
            .into_iter()
            .map(|replica| info_cache_row(replica, now_ms)),
    );
    rows
}

fn info_cache_row(instance: &crate::SentinelRedisInstance, now_ms: u64) -> RespFrame {
    let age_ms = if instance.info_refresh == 0 {
        0
    } else {
        now_ms.saturating_sub(instance.info_refresh)
    };
    let age_ms = i64::try_from(age_ms).unwrap_or(i64::MAX);
    RespFrame::Array(Some(vec![
        RespFrame::Integer(age_ms),
        RespFrame::BulkString(instance.info.as_ref().map(|info| info.as_bytes().to_vec())),
    ]))
}

fn cmd_debug(state: &mut SentinelState, args: &[&[u8]]) -> RespFrame {
    if args.is_empty() {
        return sentinel_debug_info(&state.debug_config);
    }

    let mut cursor = 0usize;
    while cursor < args.len() {
        let option_raw = String::from_utf8_lossy(args[cursor]);
        let Some(option) = canonical_sentinel_debug_key(&option_raw) else {
            return unknown_sentinel_debug_option(&option_raw);
        };
        let Some(value) = args.get(cursor + 1) else {
            return unknown_sentinel_debug_option(&option_raw);
        };
        let value = String::from_utf8_lossy(value);
        let value = match parse_positive_debug_u64(&value, &option_raw) {
            Ok(value) => value,
            Err(error) => return error,
        };
        apply_sentinel_debug_update(&mut state.debug_config, option, value);
        cursor += 2;
    }

    RespFrame::SimpleString("OK".into())
}

fn sentinel_debug_info(config: &crate::SentinelDebugConfig) -> RespFrame {
    let entries = [
        ("INFO-PERIOD", config.info_period),
        ("PING-PERIOD", config.ping_period),
        ("ASK-PERIOD", config.ask_period),
        ("PUBLISH-PERIOD", config.publish_period),
        ("DEFAULT-DOWN-AFTER", config.default_down_after),
        ("DEFAULT-FAILOVER-TIMEOUT", config.default_failover_timeout),
        ("TILT-TRIGGER", config.tilt_trigger),
        ("TILT-PERIOD", config.tilt_period),
        ("SLAVE-RECONF-TIMEOUT", config.slave_reconf_timeout),
        (
            "MIN-LINK-RECONNECT-PERIOD",
            config.min_link_reconnect_period,
        ),
        ("ELECTION-TIMEOUT", config.election_timeout),
        ("SCRIPT-MAX-RUNTIME", config.script_max_runtime),
        ("SCRIPT-RETRY-DELAY", config.script_retry_delay),
    ];

    RespFrame::Map(Some(
        entries
            .into_iter()
            .map(|(key, value)| {
                (
                    RespFrame::BulkString(Some(key.as_bytes().to_vec())),
                    integer_u64(value),
                )
            })
            .collect(),
    ))
}

fn canonical_sentinel_debug_key(option: &str) -> Option<&'static str> {
    [
        "info-period",
        "ping-period",
        "ask-period",
        "publish-period",
        "default-down-after",
        "default-failover-timeout",
        "tilt-trigger",
        "tilt-period",
        "slave-reconf-timeout",
        "min-link-reconnect-period",
        "election-timeout",
        "script-max-runtime",
        "script-retry-delay",
    ]
    .into_iter()
    .find(|key| key.eq_ignore_ascii_case(option))
}

fn parse_positive_debug_u64(value: &str, option: &str) -> Result<u64, RespFrame> {
    value
        .parse::<u64>()
        .ok()
        .filter(|parsed| *parsed > 0)
        .ok_or_else(|| {
            RespFrame::Error(format!(
                "ERR Invalid argument '{value}' for SENTINEL DEBUG '{option}'"
            ))
        })
}

fn apply_sentinel_debug_update(config: &mut crate::SentinelDebugConfig, option: &str, value: u64) {
    match option {
        "info-period" => config.info_period = value,
        "ping-period" => config.ping_period = value,
        "ask-period" => config.ask_period = value,
        "publish-period" => config.publish_period = value,
        "default-down-after" => config.default_down_after = value,
        "default-failover-timeout" => config.default_failover_timeout = value,
        "tilt-trigger" => config.tilt_trigger = value,
        "tilt-period" => config.tilt_period = value,
        "slave-reconf-timeout" => config.slave_reconf_timeout = value,
        "min-link-reconnect-period" => config.min_link_reconnect_period = value,
        "election-timeout" => config.election_timeout = value,
        "script-max-runtime" => config.script_max_runtime = value,
        "script-retry-delay" => config.script_retry_delay = value,
        _ => {}
    }
}

fn unknown_sentinel_debug_option(option: &str) -> RespFrame {
    RespFrame::Error(format!(
        "ERR Unknown option or number of arguments for SENTINEL DEBUG '{option}'"
    ))
}

fn integer_u64(value: u64) -> RespFrame {
    RespFrame::Integer(i64::try_from(value).unwrap_or(i64::MAX))
}

fn cmd_help() -> RespFrame {
    let help = vec![
        "SENTINEL <subcommand> [<arg> [value] [opt] ...]. Subcommands are:",
        "CKQUORUM <master-name>",
        "    Check if the current Sentinel configuration is able to reach the quorum",
        "    needed to failover a master and the majority needed to authorize the",
        "    failover.",
        "CONFIG SET param value [param value ...]",
        "    Set a global Sentinel configuration parameter.",
        "CONFIG GET <param> [param param param ...]",
        "    Get global Sentinel configuration parameter.",
        "DEBUG [<param> <value> ...]",
        "    Show a list of configurable time parameters and their values (milliseconds).",
        "    Or update current configurable parameters values (one or more).",
        "GET-MASTER-ADDR-BY-NAME <master-name>",
        "    Return the ip and port number of the master with that name.",
        "FAILOVER <master-name>",
        "    Manually failover a master node without asking for agreement from other",
        "    Sentinels",
        "FLUSHCONFIG",
        "    Force Sentinel to rewrite its configuration on disk, including the current",
        "    Sentinel state.",
        "INFO-CACHE <master-name>",
        "    Return last cached INFO output from masters and all its replicas.",
        "IS-MASTER-DOWN-BY-ADDR <ip> <port> <current-epoch> <runid>",
        "    Check if the master specified by ip:port is down from current Sentinel's",
        "    point of view.",
        "MASTER <master-name>",
        "    Show the state and info of the specified master.",
        "MASTERS",
        "    Show a list of monitored masters and their state.",
        "MONITOR <name> <ip> <port> <quorum>",
        "    Start monitoring a new master with the specified name, ip, port and quorum.",
        "MYID",
        "    Return the ID of the Sentinel instance.",
        "PENDING-SCRIPTS",
        "    Get pending scripts information.",
        "REMOVE <master-name>",
        "    Remove master from Sentinel's monitor list.",
        "REPLICAS <master-name>",
        "    Show a list of replicas for this master and their state.",
        "RESET <pattern>",
        "    Reset masters for specific master name matching this pattern.",
        "SENTINELS <master-name>",
        "    Show a list of Sentinel instances for this master and their state.",
        "SET <master-name> <option> <value> [<option> <value> ...]",
        "    Set configuration parameters for certain masters.",
        "SIMULATE-FAILURE [CRASH-AFTER-ELECTION] [CRASH-AFTER-PROMOTION] [HELP]",
        "    Simulate a Sentinel crash.",
        "HELP",
        "    Print this help.",
    ];
    RespFrame::Array(Some(
        help.into_iter()
            .map(|s| RespFrame::SimpleString(s.to_string()))
            .collect(),
    ))
}

fn sorted_instance_info_arrays<'a>(
    instances: impl Iterator<Item = &'a crate::SentinelRedisInstance>,
) -> Vec<RespFrame> {
    let mut instances: Vec<_> = instances.collect();
    instances.sort_by(|left, right| left.name.cmp(&right.name));
    instances.into_iter().map(instance_to_info_array).collect()
}

fn instance_to_info_array(instance: &crate::SentinelRedisInstance) -> RespFrame {
    let mut pairs = vec![
        RespFrame::BulkString(Some(b"name".to_vec())),
        RespFrame::BulkString(Some(instance.name.clone().into_bytes())),
        RespFrame::BulkString(Some(b"ip".to_vec())),
        RespFrame::BulkString(Some(instance.addr.hostname.clone().into_bytes())),
        RespFrame::BulkString(Some(b"port".to_vec())),
        RespFrame::BulkString(Some(instance.addr.port.to_string().into_bytes())),
        RespFrame::BulkString(Some(b"flags".to_vec())),
        RespFrame::BulkString(Some(flags_to_string(&instance.flags).into_bytes())),
        RespFrame::BulkString(Some(b"quorum".to_vec())),
        RespFrame::BulkString(Some(instance.quorum.to_string().into_bytes())),
        RespFrame::BulkString(Some(b"down-after-milliseconds".to_vec())),
        RespFrame::BulkString(Some(instance.down_after_period.to_string().into_bytes())),
        RespFrame::BulkString(Some(b"failover-timeout".to_vec())),
        RespFrame::BulkString(Some(instance.failover_timeout.to_string().into_bytes())),
        RespFrame::BulkString(Some(b"parallel-syncs".to_vec())),
        RespFrame::BulkString(Some(instance.parallel_syncs.to_string().into_bytes())),
    ];

    if instance.is_master() {
        if let Some(ref script) = instance.notification_script {
            pairs.push(RespFrame::BulkString(Some(b"notification-script".to_vec())));
            pairs.push(RespFrame::BulkString(Some(script.clone().into_bytes())));
        }
        if let Some(ref script) = instance.client_reconfig_script {
            pairs.push(RespFrame::BulkString(Some(
                b"client-reconfig-script".to_vec(),
            )));
            pairs.push(RespFrame::BulkString(Some(script.clone().into_bytes())));
        }
    }

    pairs.extend([
        RespFrame::BulkString(Some(b"num-slaves".to_vec())),
        RespFrame::BulkString(Some(instance.slaves.len().to_string().into_bytes())),
        RespFrame::BulkString(Some(b"num-other-sentinels".to_vec())),
        RespFrame::BulkString(Some(instance.sentinels.len().to_string().into_bytes())),
    ]);

    if let Some(ref runid) = instance.runid {
        pairs.push(RespFrame::BulkString(Some(b"runid".to_vec())));
        pairs.push(RespFrame::BulkString(Some(runid.clone().into_bytes())));
    }

    RespFrame::Array(Some(pairs))
}

fn flags_to_string(flags: &crate::InstanceFlags) -> String {
    let mut parts = Vec::new();
    if flags.contains(crate::InstanceFlags::MASTER) {
        parts.push("master");
    }
    if flags.contains(crate::InstanceFlags::SLAVE) {
        parts.push("slave");
    }
    if flags.contains(crate::InstanceFlags::SENTINEL) {
        parts.push("sentinel");
    }
    if flags.contains(crate::InstanceFlags::S_DOWN) {
        parts.push("s_down");
    }
    if flags.contains(crate::InstanceFlags::O_DOWN) {
        parts.push("o_down");
    }
    if flags.contains(crate::InstanceFlags::MASTER_DOWN) {
        parts.push("master_down");
    }
    if flags.contains(crate::InstanceFlags::FAILOVER_IN_PROGRESS) {
        parts.push("failover_in_progress");
    }
    if parts.is_empty() {
        "none".to_string()
    } else {
        parts.join(",")
    }
}

fn glob_match(pattern: &str, text: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    if pattern.starts_with('*') && pattern.ends_with('*') {
        let mid = &pattern[1..pattern.len() - 1];
        return text.contains(mid);
    }
    if let Some(suffix) = pattern.strip_prefix('*') {
        return text.ends_with(suffix);
    }
    if let Some(prefix) = pattern.strip_suffix('*') {
        return text.starts_with(prefix);
    }
    pattern == text
}

fn glob_match_ignore_ascii_case(pattern: &str, text: &str) -> bool {
    glob_match(&pattern.to_ascii_lowercase(), &text.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{InstanceFlags, ScriptJob, SentinelAddr, SentinelRedisInstance};

    fn bulk_str(value: &str) -> RespFrame {
        RespFrame::BulkString(Some(value.as_bytes().to_vec()))
    }

    fn sentinel_instance(
        name: &str,
        hostname: &str,
        port: u16,
        flags: InstanceFlags,
    ) -> SentinelRedisInstance {
        let mut instance =
            SentinelRedisInstance::new_master(name, SentinelAddr::new(hostname, port), 0);
        instance.flags = flags;
        instance
    }

    fn add_replica(
        state: &mut SentinelState,
        master_name: &str,
        replica_name: &str,
        configure: impl FnOnce(&mut SentinelRedisInstance),
    ) {
        let master_exists = state.masters.contains_key(master_name);
        let Some(master) = state.get_master_mut(master_name) else {
            assert!(master_exists, "{master_name} master exists");
            return;
        };
        let mut replica = sentinel_instance(replica_name, "127.0.0.1", 6380, InstanceFlags::SLAVE);
        configure(&mut replica);
        master.slaves.insert(replica_name.to_string(), replica);
    }

    fn list_reply_names(frame: RespFrame) -> Vec<String> {
        let RespFrame::Array(Some(instances)) = frame else {
            return Vec::new();
        };
        instances
            .into_iter()
            .filter_map(|instance| {
                let RespFrame::Array(Some(fields)) = instance else {
                    return None;
                };
                fields
                    .chunks_exact(2)
                    .find_map(|pair| match (&pair[0], &pair[1]) {
                        (RespFrame::BulkString(Some(key)), RespFrame::BulkString(Some(value)))
                            if key == b"name" =>
                        {
                            String::from_utf8(value.clone()).ok()
                        }
                        _ => None,
                    })
            })
            .collect()
    }

    fn array_len(frame: &RespFrame) -> Option<usize> {
        match frame {
            RespFrame::Array(Some(items)) => Some(items.len()),
            _ => None,
        }
    }

    fn info_field(frame: &RespFrame, key: &[u8]) -> Option<String> {
        let RespFrame::Array(Some(fields)) = frame else {
            return None;
        };
        fields
            .chunks_exact(2)
            .find_map(|pair| match (&pair[0], &pair[1]) {
                (RespFrame::BulkString(Some(name)), RespFrame::BulkString(Some(value)))
                    if name == key =>
                {
                    String::from_utf8(value.clone()).ok()
                }
                _ => None,
            })
    }

    fn debug_integer_field(frame: &RespFrame, key: &[u8]) -> Option<i64> {
        let RespFrame::Map(Some(fields)) = frame else {
            return None;
        };
        fields.iter().find_map(|(name, value)| match (name, value) {
            (RespFrame::BulkString(Some(name)), RespFrame::Integer(value))
                if name.as_slice().eq(key) =>
            {
                Some(*value)
            }
            _ => None,
        })
    }

    fn expected_info_cache_row(age_ms: i64, info: Option<&str>) -> RespFrame {
        RespFrame::Array(Some(vec![
            RespFrame::Integer(age_ms),
            RespFrame::BulkString(info.map(|value| value.as_bytes().to_vec())),
        ]))
    }

    fn is_master_down_reply(is_down: i64, leader: &str, leader_epoch: i64) -> RespFrame {
        RespFrame::Array(Some(vec![
            RespFrame::Integer(is_down),
            RespFrame::BulkString(Some(leader.as_bytes().to_vec())),
            RespFrame::Integer(leader_epoch),
        ]))
    }

    #[test]
    fn test_myid() {
        let mut state = SentinelState::new();
        let result = dispatch_sentinel_command(&mut state, &[b"MYID"]);
        assert!(matches!(result, RespFrame::BulkString(Some(_))));
    }

    #[test]
    fn test_monitor_and_masters() {
        let mut state = SentinelState::new();
        let result = dispatch_sentinel_command(
            &mut state,
            &[b"MONITOR", b"mymaster", b"127.0.0.1", b"6379", b"2"],
        );
        assert!(matches!(result, RespFrame::SimpleString(_)));

        let result = dispatch_sentinel_command(&mut state, &[b"MASTERS"]);
        assert_eq!(array_len(&result), Some(1));
    }

    #[test]
    fn sentinel_is_master_down_by_addr_reports_unknown_as_not_down() {
        let mut state = SentinelState::new();
        let result = dispatch_sentinel_command(
            &mut state,
            &[b"IS-MASTER-DOWN-BY-ADDR", b"127.0.0.1", b"6379", b"1", b"*"],
        );
        assert_eq!(result, is_master_down_reply(0, "*", 0));
    }

    #[test]
    fn sentinel_is_master_down_by_addr_checks_subjective_down_without_vote() {
        let mut state = SentinelState::new();
        assert!(state.monitor("mymaster", "127.0.0.1", 6379, 1).is_ok());
        assert!(state.get_master("mymaster").is_some());
        if let Some(master) = state.get_master_mut("mymaster") {
            master.flags.insert(InstanceFlags::S_DOWN);
        }

        let result = dispatch_sentinel_command(
            &mut state,
            &[b"IS-MASTER-DOWN-BY-ADDR", b"127.0.0.1", b"6379", b"7", b"*"],
        );
        assert_eq!(result, is_master_down_reply(1, "*", 0));
        assert_eq!(state.current_epoch, 0);
        assert_eq!(
            state
                .get_master("mymaster")
                .and_then(|master| master.leader.clone()),
            None
        );
    }

    #[test]
    fn sentinel_is_master_down_by_addr_votes_once_per_epoch() {
        let mut state = SentinelState::new();
        assert!(state.monitor("mymaster", "127.0.0.1", 6379, 1).is_ok());
        assert!(state.get_master("mymaster").is_some());
        if let Some(master) = state.get_master_mut("mymaster") {
            master.flags.insert(InstanceFlags::S_DOWN);
        }

        let first = dispatch_sentinel_command(
            &mut state,
            &[
                b"IS-MASTER-DOWN-BY-ADDR",
                b"127.0.0.1",
                b"6379",
                b"7",
                b"candidate-a",
            ],
        );
        assert_eq!(first, is_master_down_reply(1, "candidate-a", 7));
        assert_eq!(state.current_epoch, 7);

        let duplicate_epoch = dispatch_sentinel_command(
            &mut state,
            &[
                b"IS-MASTER-DOWN-BY-ADDR",
                b"127.0.0.1",
                b"6379",
                b"7",
                b"candidate-b",
            ],
        );
        assert_eq!(duplicate_epoch, is_master_down_reply(1, "candidate-a", 7));

        let next_epoch = dispatch_sentinel_command(
            &mut state,
            &[
                b"IS-MASTER-DOWN-BY-ADDR",
                b"127.0.0.1",
                b"6379",
                b"8",
                b"candidate-b",
            ],
        );
        assert_eq!(next_epoch, is_master_down_reply(1, "candidate-b", 8));
        assert_eq!(state.current_epoch, 8);
    }

    #[test]
    fn sentinel_is_master_down_by_addr_tilt_suppresses_down_but_not_vote() {
        let mut state = SentinelState::new();
        assert!(state.monitor("mymaster", "127.0.0.1", 6379, 1).is_ok());
        state.tilt = true;
        assert!(state.get_master("mymaster").is_some());
        if let Some(master) = state.get_master_mut("mymaster") {
            master.flags.insert(InstanceFlags::S_DOWN);
        }

        let result = dispatch_sentinel_command(
            &mut state,
            &[
                b"IS-MASTER-DOWN-BY-ADDR",
                b"127.0.0.1",
                b"6379",
                b"9",
                b"candidate",
            ],
        );
        assert_eq!(result, is_master_down_reply(0, "candidate", 9));
    }

    #[test]
    fn sentinel_monitor_rejects_non_positive_quorum() {
        let mut state = SentinelState::new();
        let zero = dispatch_sentinel_command(
            &mut state,
            &[b"MONITOR", b"zero", b"127.0.0.1", b"6379", b"0"],
        );
        assert_eq!(
            zero,
            RespFrame::Error("ERR Quorum must be 1 or greater.".into())
        );
        assert!(state.get_master("zero").is_none());

        let negative = dispatch_sentinel_command(
            &mut state,
            &[b"MONITOR", b"negative", b"127.0.0.1", b"6379", b"-1"],
        );
        assert_eq!(
            negative,
            RespFrame::Error("ERR Quorum must be 1 or greater.".into())
        );
        assert!(state.get_master("negative").is_none());

        let malformed = dispatch_sentinel_command(
            &mut state,
            &[b"MONITOR", b"malformed", b"127.0.0.1", b"6379", b"NaN"],
        );
        assert_eq!(
            malformed,
            RespFrame::Error("ERR Invalid quorum number".into())
        );
        assert!(state.get_master("malformed").is_none());
    }

    #[test]
    fn sentinel_monitor_rejects_hostname_when_resolution_is_disabled() {
        let mut state = SentinelState::new();
        assert!(!state.resolve_hostnames);

        let result = dispatch_sentinel_command(
            &mut state,
            &[b"MONITOR", b"badhost", b"example.local", b"6379", b"1"],
        );
        assert_eq!(
            result,
            RespFrame::Error("ERR Invalid IP address or hostname specified".into())
        );
        assert!(state.get_master("badhost").is_none());

        state.resolve_hostnames = true;
        let result = dispatch_sentinel_command(
            &mut state,
            &[b"MONITOR", b"host-ok", b"example.local", b"6379", b"1"],
        );
        assert_eq!(result, RespFrame::SimpleString("OK".into()));
        assert!(state.get_master("host-ok").is_some());
    }

    #[test]
    fn sentinel_list_replies_are_sorted_by_instance_name() {
        let mut state = SentinelState::new();
        for name in ["gamma", "alpha", "beta"] {
            let result = dispatch_sentinel_command(
                &mut state,
                &[b"MONITOR", name.as_bytes(), b"127.0.0.1", b"6379", b"2"],
            );
            assert!(matches!(result, RespFrame::SimpleString(_)));
        }

        let master_exists = state.masters.contains_key("beta");
        let Some(master) = state.get_master_mut("beta") else {
            assert!(master_exists, "beta master exists");
            return;
        };
        for name in ["replica-c", "replica-a", "replica-b"] {
            let replica = sentinel_instance(name, "127.0.0.1", 6380, InstanceFlags::SLAVE);
            master.slaves.insert(name.to_string(), replica);
        }
        for name in ["sentinel-c", "sentinel-a", "sentinel-b"] {
            let sentinel = sentinel_instance(name, "127.0.0.1", 26379, InstanceFlags::SENTINEL);
            master.sentinels.insert(name.to_string(), sentinel);
        }

        assert_eq!(
            list_reply_names(dispatch_sentinel_command(&mut state, &[b"MASTERS"])),
            ["alpha", "beta", "gamma"]
        );
        assert_eq!(
            list_reply_names(dispatch_sentinel_command(
                &mut state,
                &[b"REPLICAS", b"beta"]
            )),
            ["replica-a", "replica-b", "replica-c"]
        );
        assert_eq!(
            list_reply_names(dispatch_sentinel_command(
                &mut state,
                &[b"SENTINELS", b"beta"]
            )),
            ["sentinel-a", "sentinel-b", "sentinel-c"]
        );
    }

    #[test]
    fn test_get_master_addr() {
        let mut state = SentinelState::new();
        let _ = dispatch_sentinel_command(
            &mut state,
            &[b"MONITOR", b"mymaster", b"192.168.1.100", b"6379", b"2"],
        );

        let result =
            dispatch_sentinel_command(&mut state, &[b"GET-MASTER-ADDR-BY-NAME", b"mymaster"]);
        assert_eq!(array_len(&result), Some(2));
    }

    #[test]
    fn test_set_options() {
        let mut state = SentinelState::new();
        let _ = dispatch_sentinel_command(
            &mut state,
            &[b"MONITOR", b"mymaster", b"127.0.0.1", b"6379", b"2"],
        );

        let result = dispatch_sentinel_command(
            &mut state,
            &[b"SET", b"mymaster", b"down-after-milliseconds", b"5000"],
        );
        assert!(matches!(result, RespFrame::SimpleString(_)));

        let master = state.get_master("mymaster");
        assert!(master.is_some(), "mymaster exists");
        let Some(master) = master else {
            return;
        };
        assert_eq!(master.down_after_period, 5000);
    }

    #[test]
    fn sentinel_set_empty_auth_values_clear_credentials() {
        let mut state = SentinelState::new();
        let _ = dispatch_sentinel_command(
            &mut state,
            &[b"MONITOR", b"mymaster", b"127.0.0.1", b"6379", b"2"],
        );

        let result = dispatch_sentinel_command(
            &mut state,
            &[
                b"SET",
                b"mymaster",
                b"auth-user",
                b"agent",
                b"auth-pass",
                b"secret",
            ],
        );
        assert!(matches!(result, RespFrame::SimpleString(_)));

        let master = state.get_master("mymaster");
        assert!(master.is_some(), "mymaster exists");
        let Some(master) = master else {
            return;
        };
        assert_eq!(master.auth_user.as_deref(), Some("agent"));
        assert_eq!(master.auth_pass.as_deref(), Some("secret"));

        let result = dispatch_sentinel_command(
            &mut state,
            &[b"SET", b"mymaster", b"auth-user", b"", b"auth-pass", b""],
        );
        assert!(matches!(result, RespFrame::SimpleString(_)));

        let master = state.get_master("mymaster");
        assert!(master.is_some(), "mymaster exists");
        let Some(master) = master else {
            return;
        };
        assert_eq!(master.auth_user, None);
        assert_eq!(master.auth_pass, None);
    }

    #[test]
    fn sentinel_set_script_paths_accept_executable_and_empty_clears() {
        let mut state = SentinelState::new();
        let _ = dispatch_sentinel_command(
            &mut state,
            &[b"MONITOR", b"mymaster", b"127.0.0.1", b"6379", b"2"],
        );
        let executable_path = match std::env::current_exe() {
            Ok(path) => path.to_string_lossy().into_owned(),
            Err(_) => return,
        };
        assert!(!executable_path.is_empty(), "test executable has a path");

        let result = dispatch_sentinel_command(
            &mut state,
            &[
                b"SET",
                b"mymaster",
                b"notification-script",
                executable_path.as_bytes(),
                b"client-reconfig-script",
                executable_path.as_bytes(),
            ],
        );
        assert!(matches!(result, RespFrame::SimpleString(_)));

        let master = state.get_master("mymaster");
        assert!(master.is_some(), "mymaster exists");
        let Some(master) = master else {
            return;
        };
        assert_eq!(
            master.notification_script.as_deref(),
            Some(executable_path.as_str())
        );
        assert_eq!(
            master.client_reconfig_script.as_deref(),
            Some(executable_path.as_str())
        );

        let result = dispatch_sentinel_command(
            &mut state,
            &[
                b"SET",
                b"mymaster",
                b"notification-script",
                b"",
                b"client-reconfig-script",
                b"",
            ],
        );
        assert!(matches!(result, RespFrame::SimpleString(_)));

        let master = state.get_master("mymaster");
        assert!(master.is_some(), "mymaster exists");
        let Some(master) = master else {
            return;
        };
        assert_eq!(master.notification_script, None);
        assert_eq!(master.client_reconfig_script, None);
    }

    #[test]
    fn sentinel_set_script_paths_respect_deny_scripts_reconfig() {
        let mut state = SentinelState::new();
        state.deny_scripts_reconfig = true;
        let _ = dispatch_sentinel_command(
            &mut state,
            &[b"MONITOR", b"mymaster", b"127.0.0.1", b"6379", b"2"],
        );

        let result = dispatch_sentinel_command(
            &mut state,
            &[
                b"SET",
                b"mymaster",
                b"notification-script",
                b"/tmp/frankenredis-missing-sentinel-notification-script",
            ],
        );
        assert!(
            matches!(result, RespFrame::Error(message) if message.contains("deny-scripts-reconfig"))
        );

        let master = state.get_master("mymaster");
        assert!(master.is_some(), "mymaster exists");
        let Some(master) = master else {
            return;
        };
        assert_eq!(master.notification_script, None);
    }

    #[test]
    fn sentinel_master_replies_include_configured_script_paths() {
        let mut state = SentinelState::new();
        let _ = dispatch_sentinel_command(
            &mut state,
            &[b"MONITOR", b"mymaster", b"127.0.0.1", b"6379", b"2"],
        );
        let executable_path = match std::env::current_exe() {
            Ok(path) => path.to_string_lossy().into_owned(),
            Err(_) => return,
        };
        assert!(!executable_path.is_empty(), "test executable has a path");

        let result = dispatch_sentinel_command(
            &mut state,
            &[
                b"SET",
                b"mymaster",
                b"notification-script",
                executable_path.as_bytes(),
                b"client-reconfig-script",
                executable_path.as_bytes(),
            ],
        );
        assert!(matches!(result, RespFrame::SimpleString(_)));

        let result = dispatch_sentinel_command(&mut state, &[b"MASTER", b"mymaster"]);
        assert_eq!(
            info_field(&result, b"notification-script").as_deref(),
            Some(executable_path.as_str())
        );
        assert_eq!(
            info_field(&result, b"client-reconfig-script").as_deref(),
            Some(executable_path.as_str())
        );

        let result = dispatch_sentinel_command(&mut state, &[b"MASTERS"]);
        assert_eq!(array_len(&result), Some(1));
        let RespFrame::Array(Some(masters)) = result else {
            return;
        };
        let first = masters.first();
        assert!(first.is_some(), "MASTERS includes mymaster");
        let Some(first) = first else {
            return;
        };
        assert_eq!(
            info_field(first, b"notification-script").as_deref(),
            Some(executable_path.as_str())
        );
        assert_eq!(
            info_field(first, b"client-reconfig-script").as_deref(),
            Some(executable_path.as_str())
        );
    }

    #[test]
    fn sentinel_set_master_reboot_down_after_period_accepts_non_negative_values() {
        let mut state = SentinelState::new();
        let _ = dispatch_sentinel_command(
            &mut state,
            &[b"MONITOR", b"mymaster", b"127.0.0.1", b"6379", b"2"],
        );

        let result = dispatch_sentinel_command(
            &mut state,
            &[
                b"SET",
                b"mymaster",
                b"master-reboot-down-after-period",
                b"0",
            ],
        );
        assert!(matches!(result, RespFrame::SimpleString(_)));

        let master = state.get_master("mymaster");
        assert!(master.is_some(), "mymaster exists");
        let Some(master) = master else {
            return;
        };
        assert_eq!(master.master_reboot_down_after_period, 0);

        let result = dispatch_sentinel_command(
            &mut state,
            &[
                b"SET",
                b"mymaster",
                b"master-reboot-down-after-period",
                b"5000",
            ],
        );
        assert!(matches!(result, RespFrame::SimpleString(_)));

        let master = state.get_master("mymaster");
        assert!(master.is_some(), "mymaster exists");
        let Some(master) = master else {
            return;
        };
        assert_eq!(master.master_reboot_down_after_period, 5000);

        let result = dispatch_sentinel_command(
            &mut state,
            &[
                b"SET",
                b"mymaster",
                b"master-reboot-down-after-period",
                b"-1",
            ],
        );
        assert!(
            matches!(result, RespFrame::Error(message) if message.contains("Invalid argument"))
        );

        let master = state.get_master("mymaster");
        assert!(master.is_some(), "mymaster exists");
        let Some(master) = master else {
            return;
        };
        assert_eq!(master.master_reboot_down_after_period, 5000);
    }

    #[test]
    fn sentinel_set_rename_command_tracks_per_master_mapping() {
        let mut state = SentinelState::new();
        let _ = dispatch_sentinel_command(
            &mut state,
            &[b"MONITOR", b"mymaster", b"127.0.0.1", b"6379", b"2"],
        );

        let result = dispatch_sentinel_command(
            &mut state,
            &[
                b"SET",
                b"mymaster",
                b"rename-command",
                b"CONFIG",
                b"SENTINEL-CONFIG",
                b"auth-user",
                b"sentinel-user",
            ],
        );
        assert!(matches!(result, RespFrame::SimpleString(_)));

        let master = state.get_master("mymaster");
        assert!(master.is_some(), "mymaster exists");
        let Some(master) = master else {
            return;
        };
        assert_eq!(
            master.renamed_commands.get("config").map(String::as_str),
            Some("SENTINEL-CONFIG")
        );
        assert_eq!(master.auth_user.as_deref(), Some("sentinel-user"));

        let result = dispatch_sentinel_command(
            &mut state,
            &[b"SET", b"mymaster", b"rename-command", b"CONFIG", b"CONFIG"],
        );
        assert!(matches!(result, RespFrame::SimpleString(_)));

        let master = state.get_master("mymaster");
        assert!(master.is_some(), "mymaster exists");
        let Some(master) = master else {
            return;
        };
        assert!(!master.renamed_commands.contains_key("config"));
    }

    #[test]
    fn sentinel_set_rename_command_rejects_empty_names_and_missing_value() {
        let mut state = SentinelState::new();
        let _ = dispatch_sentinel_command(
            &mut state,
            &[b"MONITOR", b"mymaster", b"127.0.0.1", b"6379", b"2"],
        );

        for args in [
            [
                b"SET".as_slice(),
                b"mymaster".as_slice(),
                b"rename-command".as_slice(),
                b"".as_slice(),
                b"RENAMED".as_slice(),
            ],
            [
                b"SET".as_slice(),
                b"mymaster".as_slice(),
                b"rename-command".as_slice(),
                b"CONFIG".as_slice(),
                b"".as_slice(),
            ],
        ] {
            let result = dispatch_sentinel_command(&mut state, &args);
            assert!(
                matches!(result, RespFrame::Error(message) if message.contains("Invalid argument"))
            );
        }

        let result = dispatch_sentinel_command(
            &mut state,
            &[b"SET", b"mymaster", b"rename-command", b"CONFIG"],
        );
        assert!(
            matches!(result, RespFrame::Error(message) if message.contains("Unknown option or number of arguments"))
        );

        let master = state.get_master("mymaster");
        assert!(master.is_some(), "mymaster exists");
        let Some(master) = master else {
            return;
        };
        assert!(master.renamed_commands.is_empty());
    }

    #[test]
    fn sentinel_set_rejects_invalid_positive_integer_options() {
        let mut state = SentinelState::new();
        let _ = dispatch_sentinel_command(
            &mut state,
            &[b"MONITOR", b"mymaster", b"127.0.0.1", b"6379", b"2"],
        );
        let before = {
            let master = state.get_master("mymaster");
            assert!(master.is_some(), "mymaster exists");
            let Some(master) = master else {
                return;
            };
            (
                master.down_after_period,
                master.failover_timeout,
                master.parallel_syncs,
                master.quorum,
            )
        };

        for (option, value) in [
            (
                b"down-after-milliseconds".as_slice(),
                b"not-a-number".as_slice(),
            ),
            (b"failover-timeout".as_slice(), b"0".as_slice()),
            (b"parallel-syncs".as_slice(), b"-1".as_slice()),
            (b"quorum".as_slice(), b"0".as_slice()),
        ] {
            let result =
                dispatch_sentinel_command(&mut state, &[b"SET", b"mymaster", option, value]);
            assert!(
                matches!(result, RespFrame::Error(ref message) if message.contains("Invalid argument"))
            );
        }

        let master = state.get_master("mymaster");
        assert!(master.is_some(), "mymaster exists");
        let Some(master) = master else {
            return;
        };
        assert_eq!(
            (
                master.down_after_period,
                master.failover_timeout,
                master.parallel_syncs,
                master.quorum,
            ),
            before
        );
    }

    #[test]
    fn test_failover() {
        let mut state = SentinelState::new();
        state.previous_time = 1234;
        let _ = dispatch_sentinel_command(
            &mut state,
            &[b"MONITOR", b"mymaster", b"127.0.0.1", b"6379", b"2"],
        );
        add_replica(&mut state, "mymaster", "replica-1", |_| {});

        let result = dispatch_sentinel_command(&mut state, &[b"FAILOVER", b"mymaster"]);
        assert!(matches!(result, RespFrame::SimpleString(_)));
        assert_eq!(state.current_epoch, 1);

        let master = state.get_master("mymaster");
        assert!(master.is_some(), "mymaster exists");
        let Some(master) = master else {
            return;
        };
        assert!(master.flags.contains(crate::InstanceFlags::FORCE_FAILOVER));
        assert!(
            master
                .flags
                .contains(crate::InstanceFlags::FAILOVER_IN_PROGRESS)
        );
        assert_eq!(master.failover_epoch, 1);
        assert_eq!(master.failover_start_time, 1234);
        assert_eq!(master.failover_state_change_time, 1234);
        assert_eq!(master.failover_state, crate::FailoverState::WaitStart);
    }

    #[test]
    fn sentinel_pending_scripts_reports_upstream_job_fields() {
        let mut state = SentinelState::new();
        state.scripts_queue.push(ScriptJob {
            path: "/tmp/notify.sh".into(),
            args: vec!["event".into(), "mymaster".into()],
            retry_count: 2,
        });

        let result = dispatch_sentinel_command(&mut state, &[b"PENDING-SCRIPTS"]);

        assert_eq!(
            result,
            RespFrame::Array(Some(vec![RespFrame::Map(Some(vec![
                (
                    bulk_str("argv"),
                    RespFrame::Array(Some(vec![
                        bulk_str("/tmp/notify.sh"),
                        bulk_str("event"),
                        bulk_str("mymaster"),
                    ])),
                ),
                (bulk_str("flags"), bulk_str("scheduled")),
                (bulk_str("pid"), bulk_str("0")),
                (bulk_str("run-delay"), bulk_str("0")),
                (bulk_str("retry-num"), bulk_str("2")),
            ]))]))
        );
    }

    #[test]
    fn sentinel_failover_rejects_no_good_replica() {
        let mut state = SentinelState::new();
        let _ = dispatch_sentinel_command(
            &mut state,
            &[b"MONITOR", b"mymaster", b"127.0.0.1", b"6379", b"2"],
        );

        let result = dispatch_sentinel_command(&mut state, &[b"FAILOVER", b"mymaster"]);

        assert!(matches!(result, RespFrame::Error(message) if message.contains("NOGOODSLAVE")));
        assert_eq!(state.current_epoch, 0);
        let master = state.get_master("mymaster");
        assert!(master.is_some(), "mymaster exists");
        let Some(master) = master else {
            return;
        };
        assert!(!master.flags.contains(crate::InstanceFlags::FORCE_FAILOVER));
        assert_eq!(master.failover_state, crate::FailoverState::None);
    }

    #[test]
    fn sentinel_failover_rejects_existing_failover() {
        let mut state = SentinelState::new();
        let _ = dispatch_sentinel_command(
            &mut state,
            &[b"MONITOR", b"mymaster", b"127.0.0.1", b"6379", b"2"],
        );
        add_replica(&mut state, "mymaster", "replica-1", |_| {});
        let master_exists = state.masters.contains_key("mymaster");
        let Some(master) = state.get_master_mut("mymaster") else {
            assert!(master_exists, "mymaster exists");
            return;
        };
        master
            .flags
            .insert(crate::InstanceFlags::FAILOVER_IN_PROGRESS);

        let result = dispatch_sentinel_command(&mut state, &[b"FAILOVER", b"mymaster"]);

        assert!(matches!(result, RespFrame::Error(message) if message.contains("INPROG")));
        assert_eq!(state.current_epoch, 0);
    }

    #[test]
    fn sentinel_failover_rejects_unsuitable_replicas() {
        let mut state = SentinelState::new();
        let _ = dispatch_sentinel_command(
            &mut state,
            &[b"MONITOR", b"mymaster", b"127.0.0.1", b"6379", b"2"],
        );
        add_replica(&mut state, "mymaster", "priority-zero", |replica| {
            replica.slave_priority = 0;
        });
        add_replica(&mut state, "mymaster", "sdown", |replica| {
            replica.flags.insert(crate::InstanceFlags::S_DOWN);
        });
        add_replica(&mut state, "mymaster", "disconnected", |replica| {
            replica.link.disconnected = true;
        });

        let result = dispatch_sentinel_command(&mut state, &[b"FAILOVER", b"mymaster"]);

        assert!(matches!(result, RespFrame::Error(message) if message.contains("NOGOODSLAVE")));
        assert_eq!(state.current_epoch, 0);
    }

    #[test]
    fn test_glob_match() {
        assert!(glob_match("*", "anything"));
        assert!(glob_match("my*", "mymaster"));
        assert!(!glob_match("my*", "yourmaster"));
        assert!(glob_match("*master", "mymaster"));
        assert!(glob_match("*mast*", "mymaster"));
        assert!(glob_match("mymaster", "mymaster"));
        assert!(!glob_match("mymaster", "mymaster2"));
    }

    #[test]
    fn sentinel_config_get_set_handles_multiple_variables() {
        let mut state = SentinelState::new();

        let result = dispatch_sentinel_command(
            &mut state,
            &[
                b"CONFIG",
                b"SET",
                b"resolve-hostnames",
                b"yes",
                b"announce-port",
                b"1234",
            ],
        );
        assert_eq!(result, RespFrame::SimpleString("OK".into()));

        let result = dispatch_sentinel_command(
            &mut state,
            &[b"CONFIG", b"GET", b"resolve-hostnames", b"announce-port"],
        );
        assert_eq!(
            result,
            RespFrame::Map(Some(vec![
                (
                    RespFrame::BulkString(Some(b"resolve-hostnames".to_vec())),
                    RespFrame::BulkString(Some(b"yes".to_vec())),
                ),
                (
                    RespFrame::BulkString(Some(b"announce-port".to_vec())),
                    RespFrame::BulkString(Some(b"1234".to_vec())),
                ),
            ]))
        );
    }

    #[test]
    fn sentinel_config_get_deduplicates_unknowns_and_patterns() {
        let mut state = SentinelState::new();
        let result = dispatch_sentinel_command(
            &mut state,
            &[
                b"CONFIG",
                b"SET",
                b"loglevel",
                b"notice",
                b"announce-port",
                b"1234",
                b"announce-hostnames",
                b"yes",
            ],
        );
        assert_eq!(result, RespFrame::SimpleString("OK".into()));

        let result = dispatch_sentinel_command(
            &mut state,
            &[
                b"CONFIG",
                b"GET",
                b"resolve-hostnames",
                b"resolve-hostnames",
                b"does-not-exist",
            ],
        );
        assert_eq!(
            result,
            RespFrame::Map(Some(vec![(
                RespFrame::BulkString(Some(b"resolve-hostnames".to_vec())),
                RespFrame::BulkString(Some(b"no".to_vec())),
            )]))
        );

        let result = dispatch_sentinel_command(
            &mut state,
            &[b"CONFIG", b"GET", b"log*", b"*level", b"loglevel"],
        );
        assert_eq!(
            result,
            RespFrame::Map(Some(vec![(
                RespFrame::BulkString(Some(b"loglevel".to_vec())),
                RespFrame::BulkString(Some(b"notice".to_vec())),
            )]))
        );

        let result = dispatch_sentinel_command(&mut state, &[b"CONFIG", b"GET", b"announce*"]);
        assert_eq!(
            result,
            RespFrame::Map(Some(vec![
                (
                    RespFrame::BulkString(Some(b"announce-hostnames".to_vec())),
                    RespFrame::BulkString(Some(b"yes".to_vec())),
                ),
                (
                    RespFrame::BulkString(Some(b"announce-ip".to_vec())),
                    RespFrame::BulkString(Some(Vec::new())),
                ),
                (
                    RespFrame::BulkString(Some(b"announce-port".to_vec())),
                    RespFrame::BulkString(Some(b"1234".to_vec())),
                ),
            ]))
        );
    }

    #[test]
    fn sentinel_config_set_rejects_duplicates_atomically() {
        let mut state = SentinelState::new();
        let result =
            dispatch_sentinel_command(&mut state, &[b"CONFIG", b"SET", b"announce-port", b"111"]);
        assert_eq!(result, RespFrame::SimpleString("OK".into()));

        let result = dispatch_sentinel_command(
            &mut state,
            &[
                b"CONFIG",
                b"SET",
                b"resolve-hostnames",
                b"yes",
                b"announce-port",
                b"1234",
                b"announce-port",
                b"100",
            ],
        );
        assert!(
            matches!(result, RespFrame::Error(ref message) if message.contains("Duplicate argument"))
        );
        assert!(!state.resolve_hostnames);
        assert_eq!(state.announce_port, Some(111));
    }

    #[test]
    fn sentinel_config_set_rejects_bad_values_atomically() {
        let mut state = SentinelState::new();
        let result = dispatch_sentinel_command(
            &mut state,
            &[b"CONFIG", b"SET", b"resolve-hostnames", b"no"],
        );
        assert_eq!(result, RespFrame::SimpleString("OK".into()));

        let result = dispatch_sentinel_command(
            &mut state,
            &[
                b"CONFIG",
                b"SET",
                b"announce-port",
                b"-1234",
                b"resolve-hostnames",
                b"yes",
            ],
        );
        assert!(
            matches!(result, RespFrame::Error(ref message) if message.contains("Invalid value"))
        );
        assert!(!state.resolve_hostnames);
        assert_eq!(state.announce_port, None);
    }

    #[test]
    fn sentinel_config_set_reports_missing_values() {
        let mut state = SentinelState::new();
        let result = dispatch_sentinel_command(
            &mut state,
            &[
                b"CONFIG",
                b"SET",
                b"resolve-hostnames",
                b"yes",
                b"announce-port",
                b"1234",
                b"announce-ip",
            ],
        );
        assert!(
            matches!(result, RespFrame::Error(ref message) if message.contains("Missing argument 'announce-ip' value"))
        );
        assert!(!state.resolve_hostnames);
        assert_eq!(state.announce_port, None);
        assert_eq!(state.announce_ip, None);
    }

    #[test]
    fn sentinel_config_set_updates_credentials_and_loglevel() {
        let mut state = SentinelState::new();
        let result = dispatch_sentinel_command(
            &mut state,
            &[
                b"CONFIG",
                b"SET",
                b"sentinel-user",
                b"agent",
                b"sentinel-pass",
                b"secret",
                b"loglevel",
                b"WARNING",
            ],
        );
        assert_eq!(result, RespFrame::SimpleString("OK".into()));
        assert_eq!(state.sentinel_auth_user.as_deref(), Some("agent"));
        assert_eq!(state.sentinel_auth_pass.as_deref(), Some("secret"));
        assert_eq!(state.loglevel, "warning");

        let result = dispatch_sentinel_command(
            &mut state,
            &[
                b"CONFIG",
                b"SET",
                b"sentinel-user",
                b"",
                b"sentinel-pass",
                b"",
            ],
        );
        assert_eq!(result, RespFrame::SimpleString("OK".into()));
        assert_eq!(state.sentinel_auth_user, None);
        assert_eq!(state.sentinel_auth_pass, None);
    }

    #[test]
    fn sentinel_simulate_failure_sets_and_clears_flags() {
        let mut state = SentinelState::new();

        let result = dispatch_sentinel_command(
            &mut state,
            &[
                b"SIMULATE-FAILURE",
                b"crash-after-election",
                b"CRASH-AFTER-PROMOTION",
            ],
        );
        assert_eq!(result, RespFrame::SimpleString("OK".into()));
        assert!(
            state
                .simfailure_flags
                .contains(crate::SimFailureFlags::CRASH_AFTER_ELECTION)
        );
        assert!(
            state
                .simfailure_flags
                .contains(crate::SimFailureFlags::CRASH_AFTER_PROMOTION)
        );

        let result = dispatch_sentinel_command(&mut state, &[b"SIMULATE-FAILURE"]);
        assert_eq!(result, RespFrame::SimpleString("OK".into()));
        assert_eq!(state.simfailure_flags, crate::SimFailureFlags::empty());
    }

    #[test]
    fn sentinel_simulate_failure_help_lists_supported_modes() {
        let mut state = SentinelState::new();
        let result = dispatch_sentinel_command(&mut state, &[b"SIMULATE-FAILURE", b"HELP"]);
        assert_eq!(
            result,
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"crash-after-election".to_vec())),
                RespFrame::BulkString(Some(b"crash-after-promotion".to_vec())),
            ]))
        );
        assert_eq!(state.simfailure_flags, crate::SimFailureFlags::empty());
    }

    #[test]
    fn sentinel_simulate_failure_rejects_unknown_after_resetting_flags() {
        let mut state = SentinelState::new();
        let result =
            dispatch_sentinel_command(&mut state, &[b"SIMULATE-FAILURE", b"crash-after-election"]);
        assert_eq!(result, RespFrame::SimpleString("OK".into()));

        let result = dispatch_sentinel_command(&mut state, &[b"SIMULATE-FAILURE", b"bad-mode"]);
        assert_eq!(
            result,
            RespFrame::Error("ERR Unknown failure simulation specified".into())
        );
        assert_eq!(state.simfailure_flags, crate::SimFailureFlags::empty());
    }

    #[test]
    fn sentinel_debug_returns_full_timing_map() {
        let mut state = SentinelState::new();
        let result = dispatch_sentinel_command(&mut state, &[b"DEBUG"]);

        assert!(matches!(result, RespFrame::Map(Some(_))));
        let RespFrame::Map(Some(entries)) = &result else {
            return;
        };
        assert_eq!(entries.len(), 13);
        assert_eq!(
            debug_integer_field(&result, b"INFO-PERIOD"),
            Some(crate::INFO_PERIOD_MS as i64)
        );
        assert_eq!(
            debug_integer_field(&result, b"PING-PERIOD"),
            Some(crate::PING_PERIOD_MS as i64)
        );
        assert_eq!(
            debug_integer_field(&result, b"DEFAULT-DOWN-AFTER"),
            Some(crate::DEFAULT_DOWN_AFTER_MS as i64)
        );
        assert_eq!(
            debug_integer_field(&result, b"SCRIPT-RETRY-DELAY"),
            Some(crate::SCRIPT_RETRY_DELAY_MS as i64)
        );
    }

    #[test]
    fn sentinel_debug_updates_positive_timing_values_and_tilt_uses_them() {
        let mut state = SentinelState::new();

        let result = dispatch_sentinel_command(
            &mut state,
            &[
                b"DEBUG",
                b"ping-period",
                b"250",
                b"tilt-trigger",
                b"50",
                b"tilt-period",
                b"75",
            ],
        );
        assert_eq!(result, RespFrame::SimpleString("OK".into()));

        let result = dispatch_sentinel_command(&mut state, &[b"DEBUG"]);
        assert_eq!(debug_integer_field(&result, b"PING-PERIOD"), Some(250));
        assert_eq!(debug_integer_field(&result, b"TILT-TRIGGER"), Some(50));
        assert_eq!(debug_integer_field(&result, b"TILT-PERIOD"), Some(75));

        state.check_tilt(1000);
        state.check_tilt(1060);
        assert!(state.tilt);
        state.check_tilt(1136);
        assert!(!state.tilt);
    }

    #[test]
    fn sentinel_debug_rejects_unknown_missing_and_non_positive_values() {
        let mut state = SentinelState::new();

        let result = dispatch_sentinel_command(&mut state, &[b"DEBUG", b"ping-period", b"0"]);
        assert!(
            matches!(result, RespFrame::Error(message) if message.contains("Invalid argument"))
        );
        assert_eq!(state.debug_config.ping_period, crate::PING_PERIOD_MS);

        let result = dispatch_sentinel_command(&mut state, &[b"DEBUG", b"ping-period"]);
        assert!(
            matches!(result, RespFrame::Error(message) if message.contains("Unknown option or number of arguments"))
        );

        let result = dispatch_sentinel_command(&mut state, &[b"DEBUG", b"not-an-option", b"1"]);
        assert!(
            matches!(result, RespFrame::Error(message) if message.contains("Unknown option or number of arguments"))
        );
    }

    #[test]
    fn test_help() {
        let mut state = SentinelState::new();
        let result = dispatch_sentinel_command(&mut state, &[b"HELP"]);
        assert!(matches!(result, RespFrame::Array(Some(_))));
        let lines = if let RespFrame::Array(Some(lines)) = result {
            lines
        } else {
            Vec::new()
        };

        assert_eq!(
            lines.first(),
            Some(&RespFrame::SimpleString(
                "SENTINEL <subcommand> [<arg> [value] [opt] ...]. Subcommands are:".into()
            ))
        );
        assert_eq!(
            lines
                .iter()
                .filter(|line| matches!(line, RespFrame::SimpleString(_)))
                .count(),
            lines.len()
        );
        assert!(lines.contains(&RespFrame::SimpleString("CKQUORUM <master-name>".into())));
        assert!(lines.contains(&RespFrame::SimpleString(
            "    Check if the current Sentinel configuration is able to reach the quorum".into()
        )));
        assert!(lines.contains(&RespFrame::SimpleString(
            "SIMULATE-FAILURE [CRASH-AFTER-ELECTION] [CRASH-AFTER-PROMOTION] [HELP]".into()
        )));
        assert_eq!(
            lines[lines.len() - 2],
            RespFrame::SimpleString("HELP".into())
        );
        assert_eq!(
            lines.last(),
            Some(&RespFrame::SimpleString("    Print this help.".into()))
        );
    }

    #[test]
    fn sentinel_info_cache_returns_all_masters_with_self_and_replica_rows() {
        let mut state = SentinelState::new();
        state.previous_time = 10_000;
        let _ = dispatch_sentinel_command(
            &mut state,
            &[b"MONITOR", b"zeta", b"127.0.0.2", b"6379", b"2"],
        );
        let _ = dispatch_sentinel_command(
            &mut state,
            &[b"MONITOR", b"alpha", b"127.0.0.1", b"6380", b"1"],
        );

        {
            assert!(state.get_master("alpha").is_some());
            let Some(alpha) = state.get_master_mut("alpha") else {
                return;
            };
            alpha.info_refresh = 9_000;
            alpha.info = Some("role:master\r\nrun_id:alpha\r\n".to_string());

            let mut replica_b =
                sentinel_instance("replica-b", "127.0.0.11", 6382, InstanceFlags::SLAVE);
            replica_b.info_refresh = 9_500;
            replica_b.info = Some("role:slave\r\nrun_id:replica-b\r\n".to_string());
            alpha.slaves.insert("replica-b".to_string(), replica_b);

            let mut replica_a =
                sentinel_instance("replica-a", "127.0.0.10", 6381, InstanceFlags::SLAVE);
            replica_a.info_refresh = 0;
            replica_a.info = None;
            alpha.slaves.insert("replica-a".to_string(), replica_a);
        }

        let result = dispatch_sentinel_command(&mut state, &[b"INFO-CACHE"]);
        assert_eq!(
            result,
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"alpha".to_vec())),
                RespFrame::Array(Some(vec![
                    expected_info_cache_row(1_000, Some("role:master\r\nrun_id:alpha\r\n")),
                    expected_info_cache_row(0, None),
                    expected_info_cache_row(500, Some("role:slave\r\nrun_id:replica-b\r\n")),
                ])),
                RespFrame::BulkString(Some(b"zeta".to_vec())),
                RespFrame::Array(Some(vec![expected_info_cache_row(0, None)])),
            ]))
        );
    }

    #[test]
    fn sentinel_info_cache_filters_requested_masters_and_ignores_unknown_names() {
        let mut state = SentinelState::new();
        state.previous_time = 2_000;
        let _ = dispatch_sentinel_command(
            &mut state,
            &[b"MONITOR", b"alpha", b"127.0.0.1", b"6379", b"1"],
        );
        let _ = dispatch_sentinel_command(
            &mut state,
            &[b"MONITOR", b"beta", b"127.0.0.2", b"6380", b"1"],
        );

        let result =
            dispatch_sentinel_command(&mut state, &[b"INFO-CACHE", b"missing", b"beta", b"beta"]);
        assert_eq!(
            result,
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"beta".to_vec())),
                RespFrame::Array(Some(vec![expected_info_cache_row(0, None)])),
            ]))
        );
    }
}
