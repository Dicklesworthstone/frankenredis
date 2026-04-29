#![forbid(unsafe_code)]

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
        "MONITOR" => cmd_monitor(state, &args[1..]),
        "REMOVE" => cmd_remove(state, &args[1..]),
        "SET" => cmd_set(state, &args[1..]),
        "RESET" => cmd_reset(state, &args[1..]),
        "GET-MASTER-ADDR-BY-NAME" => cmd_get_master_addr(state, &args[1..]),
        "CKQUORUM" => cmd_ckquorum(state, &args[1..]),
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
    let quorum: u32 = match String::from_utf8_lossy(args[3]).parse() {
        Ok(q) => q,
        Err(_) => return RespFrame::Error("ERR Invalid quorum number".into()),
    };

    match state.monitor(name.as_ref(), ip.as_ref(), port, quorum) {
        Ok(()) => RespFrame::SimpleString("OK".into()),
        Err(e) => RespFrame::Error(e.into()),
    }
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
    if args.len() < 3 || args.len().is_multiple_of(2) {
        return RespFrame::Error("ERR wrong number of arguments for 'sentinel set' command".into());
    }
    let name = String::from_utf8_lossy(args[0]);
    let master = match state.get_master_mut(&name) {
        Some(m) => m,
        None => return RespFrame::Error(format!("ERR No such master with that name: {}", name)),
    };

    let mut i = 1;
    while i + 1 < args.len() {
        let option_raw = String::from_utf8_lossy(args[i]);
        let option = option_raw.to_ascii_lowercase();
        let value = String::from_utf8_lossy(args[i + 1]);

        match option.as_str() {
            "down-after-milliseconds" => {
                master.down_after_period = match parse_positive_u64(&value, &option_raw) {
                    Ok(parsed) => parsed,
                    Err(error) => return error,
                };
            }
            "failover-timeout" => {
                master.failover_timeout = match parse_positive_u64(&value, &option_raw) {
                    Ok(parsed) => parsed,
                    Err(error) => return error,
                };
            }
            "parallel-syncs" => {
                master.parallel_syncs = match parse_positive_u32(&value, &option_raw) {
                    Ok(parsed) => parsed,
                    Err(error) => return error,
                };
            }
            "quorum" => {
                master.quorum = match parse_positive_u32(&value, &option_raw) {
                    Ok(parsed) => parsed,
                    Err(error) => return error,
                };
            }
            "auth-pass" => {
                master.auth_pass = Some(value.into_owned());
            }
            "auth-user" => {
                master.auth_user = Some(value.into_owned());
            }
            _ => {
                return RespFrame::Error(format!("ERR Unknown option '{}'", option));
            }
        }
        i += 2;
    }
    RespFrame::SimpleString("OK".into())
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

fn cmd_flushconfig(_state: &SentinelState) -> RespFrame {
    RespFrame::SimpleString("OK".into())
}

fn cmd_failover(state: &mut SentinelState, args: &[&[u8]]) -> RespFrame {
    if args.len() != 1 {
        return wrong_arity("sentinel failover");
    }
    let name = String::from_utf8_lossy(args[0]);
    match state.get_master_mut(&name) {
        Some(master) => {
            use crate::{FailoverState, InstanceFlags};
            master.flags.insert(InstanceFlags::FORCE_FAILOVER);
            master.failover_state = FailoverState::WaitStart;
            RespFrame::SimpleString("OK".into())
        }
        None => RespFrame::Error(format!("ERR No such master with that name: {}", name)),
    }
}

fn cmd_pending_scripts(state: &SentinelState) -> RespFrame {
    let scripts: Vec<RespFrame> = state
        .scripts_queue
        .iter()
        .map(|script| {
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"path".to_vec())),
                RespFrame::BulkString(Some(script.path.clone().into_bytes())),
            ]))
        })
        .collect();
    RespFrame::Array(Some(scripts))
}

fn cmd_info_cache(_state: &SentinelState, args: &[&[u8]]) -> RespFrame {
    if args.len() != 1 {
        return wrong_arity("sentinel info-cache");
    }
    RespFrame::Array(Some(vec![]))
}

fn cmd_debug(_state: &SentinelState, _args: &[&[u8]]) -> RespFrame {
    RespFrame::Array(Some(vec![
        RespFrame::BulkString(Some(b"ping-period".to_vec())),
        RespFrame::Integer(crate::PING_PERIOD_MS as i64),
        RespFrame::BulkString(Some(b"info-period".to_vec())),
        RespFrame::Integer(crate::INFO_PERIOD_MS as i64),
        RespFrame::BulkString(Some(b"tilt-trigger".to_vec())),
        RespFrame::Integer(crate::TILT_TRIGGER_MS as i64),
        RespFrame::BulkString(Some(b"tilt-period".to_vec())),
        RespFrame::Integer(crate::TILT_PERIOD_MS as i64),
    ]))
}

fn cmd_help() -> RespFrame {
    let help = vec![
        "SENTINEL MYID",
        "SENTINEL MASTERS",
        "SENTINEL MASTER <name>",
        "SENTINEL REPLICAS <name>",
        "SENTINEL SENTINELS <name>",
        "SENTINEL MONITOR <name> <ip> <port> <quorum>",
        "SENTINEL REMOVE <name>",
        "SENTINEL SET <name> <option> <value> ...",
        "SENTINEL RESET <pattern>",
        "SENTINEL GET-MASTER-ADDR-BY-NAME <name>",
        "SENTINEL CKQUORUM <name>",
        "SENTINEL FLUSHCONFIG",
        "SENTINEL FAILOVER <name>",
        "SENTINEL PENDING-SCRIPTS",
        "SENTINEL INFO-CACHE <name>",
        "SENTINEL DEBUG [<param> <value> ...]",
    ];
    RespFrame::Array(Some(
        help.into_iter()
            .map(|s| RespFrame::BulkString(Some(s.as_bytes().to_vec())))
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
        RespFrame::BulkString(Some(b"num-slaves".to_vec())),
        RespFrame::BulkString(Some(instance.slaves.len().to_string().into_bytes())),
        RespFrame::BulkString(Some(b"num-other-sentinels".to_vec())),
        RespFrame::BulkString(Some(instance.sentinels.len().to_string().into_bytes())),
    ];

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{InstanceFlags, SentinelAddr, SentinelRedisInstance};

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
        let _ = dispatch_sentinel_command(
            &mut state,
            &[b"MONITOR", b"mymaster", b"127.0.0.1", b"6379", b"2"],
        );

        let result = dispatch_sentinel_command(&mut state, &[b"FAILOVER", b"mymaster"]);
        assert!(matches!(result, RespFrame::SimpleString(_)));

        let master = state.get_master("mymaster");
        assert!(master.is_some(), "mymaster exists");
        let Some(master) = master else {
            return;
        };
        assert!(master.flags.contains(crate::InstanceFlags::FORCE_FAILOVER));
        assert_eq!(master.failover_state, crate::FailoverState::WaitStart);
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
    fn test_help() {
        let mut state = SentinelState::new();
        let result = dispatch_sentinel_command(&mut state, &[b"HELP"]);
        assert!(array_len(&result).is_some_and(|len| len > 0));
    }
}
